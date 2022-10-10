mod config;
mod execute;
mod fixtures;

use clap::Parser;
use config::{ListOptions, ProcessArgs, RunOptions};
use phd_framework::artifacts::ArtifactStore;
use phd_tests::phd_testcase::TestContext;
use tracing::{debug, info, warn};
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Registry};

use crate::execute::ExecutionStats;
use crate::fixtures::TestFixtures;

fn main() {
    let runner_args = ProcessArgs::parse();
    set_tracing_subscriber(&runner_args);

    let state_write_guard = phd_framework::host_api::enable_vmm_state_writes();
    if let Err(e) = state_write_guard {
        warn!(
            error = ?e,
            "Failed to enable VMM state writes, migration tests may not work",
        );
    }

    info!(?runner_args);

    match &runner_args.command {
        config::Command::Run(opts) => {
            let exit_code = run_tests(opts).tests_failed;
            debug!(exit_code);
            std::process::exit(exit_code.try_into().unwrap());
        }
        config::Command::List(opts) => list_tests(opts),
    }
}

fn run_tests(run_opts: &RunOptions) -> ExecutionStats {
    let artifact_store = ArtifactStore::from_file(
        &run_opts.artifact_toml_path,
        run_opts.artifact_directory.clone(),
    )
    .unwrap();

    // Convert the command-line config and artifact store into a VM factory
    // definition.
    let mut config_toml_path = run_opts.tmp_directory.clone();
    config_toml_path.push("vm_config.toml");
    let factory_config = phd_framework::test_vm::factory::FactoryOptions {
        propolis_server_path: run_opts
            .propolis_server_cmd
            .to_string_lossy()
            .to_string(),
        tmp_directory: run_opts.tmp_directory.clone(),
        server_log_mode: run_opts.server_logging_mode,
        default_bootrom_artifact: run_opts.default_bootrom_artifact.clone(),
        default_guest_cpus: run_opts.default_guest_cpus,
        default_guest_memory_mib: run_opts.default_guest_memory_mib,
        server_port_range: 9000..10000,
    };

    // The VM factory config and artifact store are enough to create a test
    // context to pass to test cases and a set of fixtures.
    let ctx = TestContext {
        default_guest_image_artifact: run_opts.default_guest_artifact.clone(),
        vm_factory: phd_framework::test_vm::factory::VmFactory::new(
            factory_config,
            &artifact_store,
        )
        .unwrap(),
        disk_factory: phd_framework::disk::DiskFactory::new(
            &run_opts.tmp_directory,
            &artifact_store,
        ),
    };
    let fixtures = TestFixtures::new(&artifact_store, &ctx).unwrap();

    // Run the tests and print results.
    let execution_stats = execute::run_tests_with_ctx(&ctx, fixtures, run_opts);
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

    execution_stats
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
