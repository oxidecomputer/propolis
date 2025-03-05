// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod config;
mod execute;
mod fixtures;

use clap::Parser;
use config::{ListOptions, ProcessArgs, RunOptions};
use phd_tests::phd_testcase::{Framework, FrameworkParameters};
use phd_framework::log_config::{LogConfig, LogFormat};
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
            let exit_code = run_tests(opts, &runner_args).await?.tests_failed;
            debug!(exit_code);
            std::process::exit(exit_code.try_into().unwrap());
        }
        config::Command::List(opts) => list_tests(opts),
    }

    Ok(())
}

async fn run_tests(run_opts: &RunOptions, runner_args: &ProcessArgs) -> anyhow::Result<ExecutionStats> {
    let ctx_params = FrameworkParameters {
        propolis_server_path: run_opts.propolis_server_cmd.clone(),
        crucible_downstairs: run_opts.crucible_downstairs()?,
        base_propolis: run_opts.base_propolis(),
        tmp_directory: run_opts.tmp_directory.clone(),
        artifact_directory: run_opts.artifact_directory(),
        artifact_toml: run_opts.artifact_toml_path.clone(),
        // We have to synthesize an actual LogConfig for the test because the
        // log format - half of the config - is specified earlier to indicate
        // log formatting for the runner itself. Reuse that setting to influence
        // the formatting for tasks started by the runner during tests.
        log_config: LogConfig {
            output_mode: run_opts.output_mode,
            log_format: if runner_args.emit_bunyan {
                LogFormat::Bunyan
            } else {
                LogFormat::Plain
            }
        },
        default_guest_cpus: run_opts.default_guest_cpus,
        default_guest_memory_mib: run_opts.default_guest_memory_mib,
        default_guest_os_artifact: run_opts.default_guest_artifact.clone(),
        default_bootrom_artifact: run_opts.default_bootrom_artifact.clone(),
        port_range: 9000..10000,
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

    // Run the tests and print results.
    let execution_stats =
        execute::run_tests_with_ctx(&ctx, fixtures, run_opts).await;
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
