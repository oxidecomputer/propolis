// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod config;
mod execute;
mod fixtures;

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

async fn run_tests(run_opts: &RunOptions) -> anyhow::Result<ExecutionStats> {
    let parallelism = run_opts.parallelism.unwrap_or_else(|| {
        // Assume no test starts more than 4 VMs. This is a really conservative
        // guess to make sure we don't cause tests to fail simply because we ran
        // too many at once.
        let cpus_per_runner = run_opts.default_guest_cpus as u64 * 4;
        let memory_mib_per_runner = run_opts.default_guest_memory_mib * 4;

        // I assume there's a smarter way to do this in illumos. sorry!!
        let lgrpinfo = std::process::Command::new("/usr/bin/lgrpinfo")
            .args(["-mc", "-u", "m"])
            .output()
            .expect("can run lgrpinfo");
        assert_eq!(lgrpinfo.status.code(), Some(0));
        let lgrpinfo_output_str =
            String::from_utf8(lgrpinfo.stdout).expect("utf8 output");
        let lines: Vec<&str> = lgrpinfo_output_str.split("\n").collect();
        let cpuline = lines[1];
        let cpu_range = cpuline
            .strip_prefix("\tCPUS: ")
            .expect("`CPUs` line starts as expected");
        let mut cpu_range_parts = cpu_range.split("-");
        let cpu_low = cpu_range_parts.next().expect("range has a low");
        let cpu_high = cpu_range_parts.next().expect("range has a high");
        let ncpus = cpu_high.parse::<u64>().expect("can parse cpu_low")
            - cpu_low.parse::<u64>().expect("can parse cpu_high");

        let lim_by_cpus = ncpus / cpus_per_runner;

        let memoryline = lines[2];
        let memory = memoryline
            .strip_prefix("\tMemory: ")
            .expect("`Memory` line starts as expected");
        let installed = memory
            .split(",")
            .next()
            .expect("memory line is comma-separated elements");
        let installed_mb = installed
            .strip_prefix("installed ")
            .expect("memory line starts with installed")
            .strip_suffix("M")
            .expect("memory line ends with M")
            .parse::<u64>()
            .expect("can parse memory MB");

        let lim_by_mem = installed_mb / memory_mib_per_runner;

        std::cmp::min(lim_by_cpus as u16, lim_by_mem as u16)
    });

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
