// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod config;
mod execute;
mod fixtures;

use anyhow::{bail, Context};
use clap::Parser;
use config::{ListOptions, ProcessArgs, RunOptions};
use phd_tests::phd_testcase::{Framework, FrameworkParameters};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Registry};

use crate::execute::ExecutionStats;
use crate::fixtures::TestFixtures;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let runner_args = ProcessArgs::parse();
    set_tracing_subscriber(&runner_args);

    let state_write_guard = phd_framework::host_api::set_vmm_globals();
    if let Err(e) = state_write_guard {
        warn!(
            error = ?e,
            "Failed to enable one or more kernel options, some tests may not work",
        );
    }

    info!(?runner_args);

    match &runner_args.command {
        config::Command::Run(opts) => {
            let exit_code = run_tests(opts).await?.tests_failed;
            debug!(exit_code);
            std::process::exit(exit_code.try_into().unwrap());
        }
        config::Command::List(opts) => list_tests(opts),
    }

    Ok(())
}

fn guess_max_reasonable_parallelism(
    default_guest_cpus: u8,
    default_guest_memory_mib: u64,
) -> anyhow::Result<u16> {
    // Assume no test starts more than 3 VMs. This is a really conservative
    // guess to make sure we don't cause tests to fail simply because we ran
    // too many at once.
    const MAX_VMS_GUESS: u64 = 3;
    let cpus_per_runner = default_guest_cpus as u64 * MAX_VMS_GUESS;
    let memory_mib_per_runner =
        (default_guest_memory_mib * MAX_VMS_GUESS) as usize;

    /// Miniscule wrapper for `sysconf(3C)` calls that checks errors.
    fn sysconf(cfg: i32) -> std::io::Result<i64> {
        // Safety: sysconf is an FFI call but we don't change any system
        // state, it won't cause unwinding, etc.
        let res = unsafe { libc::sysconf(cfg) };
        // For the handful of variables that can be queried, the variable is
        // defined and won't error. Technically if the variable is
        // unsupported, `-1` is returned without changing `errno`. In such
        // cases, returning errno might be misleading!
        //
        // Instead of trying to disambiguate this, and in the knowledge
        // these calls as we make them should never fail, just fall back to
        // a more general always-factually-correct.
        if res == -1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("could not get sysconf({})", cfg),
            ));
        }
        Ok(res)
    }

    let online_cpus = sysconf(libc::_SC_NPROCESSORS_ONLN)
        .expect("can get number of online processors")
        as u64;
    // We're assuming that the system running tests is relatively idle other
    // than the test runner itself. Overprovisioning CPUs will make everyone
    // sad but should not fail tests, at least...
    let lim_by_cpus = online_cpus / cpus_per_runner;
    info!(
        "parallelism by cpu count: {} ({} / {})",
        lim_by_cpus, online_cpus, cpus_per_runner
    );

    let ctl = bhyve_api::VmmCtlFd::open()?;
    let reservoir =
        ctl.reservoir_query().context("failed to query reservoir")?;
    let mut vmm_mem_limit = reservoir.vrq_free_sz;

    // The reservoir will be 0MiB by default if the system has not been
    // configured with a particular size.
    if reservoir.vrq_alloc_sz == 0 {
        // If the reservoir is not configured, we'll try to make do with
        // system memory and implore someone to earmark memory for test VMs in
        // the future.
        let page_size: usize = sysconf(libc::_SC_PAGESIZE)
            .expect("can get page size")
            .try_into()
            .expect("page size is reasonable");
        let total_pages: usize = sysconf(libc::_SC_PHYS_PAGES)
            .expect("can get physical pages in the system")
            .try_into()
            .expect("physical page count is reasonable");

        const MB: usize = 1024 * 1024;

        let installed_mb = page_size * total_pages / MB;
        // /!\ Arbitrary choice warning /!\
        //
        // It would be a little rude to spawn so many VMs that we cause the
        // system running tests to empty the whole ARC and swap. If there's no
        // reservior, though, we're gonna use *some* amount of memory that isn't
        // explicitly earmarked for bhyve, though. 1/4th is just a "feels ok"
        // fraction.
        vmm_mem_limit = installed_mb / 4;

        warn!(
            "phd-runner sees the VMM reservior is unconfigured, and will use \
             up to 25% of system memory ({}MiB) for test VMs. Please consider \
             using `cargo run --bin rsrvrctl set <size MiB>` to set aside \
             memory for test VMs.",
            vmm_mem_limit
        );
    }

    let lim_by_mem = vmm_mem_limit / memory_mib_per_runner;
    info!(
        "parallelism by memory: {} ({} / {})",
        lim_by_mem, vmm_mem_limit, memory_mib_per_runner
    );

    Ok(std::cmp::min(lim_by_cpus as u16, lim_by_mem as u16))
}

async fn run_tests(run_opts: &RunOptions) -> anyhow::Result<ExecutionStats> {
    let parallelism = if let Some(parallelism) = run_opts.parallelism {
        if parallelism == 0 {
            bail!("Parallelism of 0 was requested; cannot run tests under these conditions!");
        }
        parallelism
    } else {
        let res = guess_max_reasonable_parallelism(
            run_opts.default_guest_cpus,
            run_opts.default_guest_memory_mib,
        )?;
        if res == 0 {
            bail!(
                "Inferred a parallelism of 0; this is probably because there \
                is not much available memory for test VMs? Consider checking \
                reservoir configuration."
            );
        }
        res
    };

    info!("running tests with max parallelism of {}", parallelism);

    // /!\ Arbitrary choice warning /!\
    //
    // We probably only need a half dozen ports at most for any test. 200 is an
    // incredible overallocation to never have to worry about the problem. As
    // long as we don't have hundreds of test runners running concurrently.
    const PORT_RANGE_PER_RUNNER: u16 = 125;

    let mut runners = Vec::new();

    // Create up to parallelism collections of PHD framework settings.  For the
    // most part we can reuse the same settings for all test-running tasks -
    // state is not mutably shared. The important exception is port ranges for
    // servers PHD will run, where we want non-overlapping port ranges to ensure
    // that concurrent tests don't accidentally use a neighbor's servers.
    for i in 0..parallelism {
        let port_range = (9000 + PORT_RANGE_PER_RUNNER * i)
            ..(9000 + PORT_RANGE_PER_RUNNER * (i + 1));
        let ctx_params = FrameworkParameters {
            propolis_server_path: run_opts.propolis_server_cmd.clone(),
            crucible_downstairs: run_opts.crucible_downstairs()?,
            base_propolis: run_opts.base_propolis(),
            tmp_directory: run_opts.tmp_directory.clone(),
            artifact_directory: run_opts.artifact_directory(),
            artifact_toml: run_opts.artifact_toml_path.clone(),
            server_log_mode: run_opts.server_logging_mode,
            default_guest_cpus: run_opts.default_guest_cpus,
            default_guest_memory_mib: run_opts.default_guest_memory_mib,
            default_guest_os_artifact: run_opts.default_guest_artifact.clone(),
            default_bootrom_artifact: run_opts.default_bootrom_artifact.clone(),
            port_range,
            max_buildomat_wait: Duration::from_secs(
                run_opts.max_buildomat_wait_secs,
            ),
        };

        let ctx = Arc::new(
            Framework::new(ctx_params)
                .await
                .expect("should be able to set up a test context"),
        );

        let fixtures = TestFixtures::new(ctx.clone()).unwrap();
        runners.push((ctx, fixtures));
    }

    // Run the tests and print results.
    let execution_stats =
        execute::run_tests_with_ctx(&mut runners, run_opts).await;
    if !execution_stats.failed_test_cases.is_empty() {
        println!("\nfailures:");
        for tc in &execution_stats.failed_test_cases {
            println!("    {}", tc.fully_qualified_name());
        }
        println!();
    }

    println!(
        "test result: {}. {} passed; {} failed; {} skipped; {} not run; \
        finished in {:.2}s\n",
        if execution_stats.tests_failed != 0 { "FAILED" } else { "ok" },
        execution_stats.tests_passed,
        execution_stats.tests_failed,
        execution_stats.tests_skipped,
        execution_stats.tests_not_run,
        execution_stats.duration.as_secs_f64()
    );

    Ok(execution_stats)
}

fn list_tests(list_opts: &ListOptions) {
    println!("Tests enabled after applying filters:\n");

    let mut count = 0;
    for tc in phd_tests::phd_testcase::filtered_test_cases(
        &list_opts.include_filter,
        &list_opts.exclude_filter,
    ) {
        println!("    {}", tc.fully_qualified_name());
        count += 1
    }

    println!("\n{} test(s) selected", count);
}

fn set_tracing_subscriber(args: &ProcessArgs) {
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into());
    let subscriber = Registry::default().with(filter.from_env_lossy());
    if args.emit_bunyan {
        let bunyan_layer =
            BunyanFormattingLayer::new("phd-runner".into(), std::io::stdout);
        let subscriber = subscriber.with(JsonStorageLayer).with(bunyan_layer);
        tracing::subscriber::set_global_default(subscriber).unwrap();
    } else {
        let stdout_log = tracing_subscriber::fmt::layer()
            .with_line_number(true)
            .with_ansi(!args.disable_ansi);
        let subscriber = subscriber.with(stdout_log);
        tracing::subscriber::set_global_default(subscriber).unwrap();
    }
}
