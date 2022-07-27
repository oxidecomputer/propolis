mod config;
mod execute;
mod filter;
mod fixtures;
pub(crate) mod zfs;

use clap::Parser;
use config::{ProcessArgs, RunOptions};
use phd_framework::artifacts::ArtifactStore;
use phd_tests::phd_testcase::TestContext;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Registry};

use crate::fixtures::TestFixtures;

fn main() {
    // Set up a tracing subscriber.
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into());
    let stdout_log = tracing_subscriber::fmt::layer().with_line_number(true);
    let subscriber =
        Registry::default().with(filter.from_env_lossy()).with(stdout_log);
    tracing::subscriber::set_global_default(subscriber).unwrap();

    // Set up global state: the command-line config and the artifact store.
    let runner_args = ProcessArgs::parse();
    info!(?runner_args);

    match &runner_args.command {
        config::Command::Run(opts) => run_tests(&runner_args, opts),
        config::Command::List => list_tests(runner_args),
    }
}

fn run_tests(process_args: &ProcessArgs, run_opts: &RunOptions) {
    let artifact_store =
        ArtifactStore::from_file(&run_opts.artifact_toml_path).unwrap();

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
        default_guest_image_artifact: run_opts.default_guest_artifact.clone(),
        default_bootrom_artifact: run_opts.default_bootrom_artifact.clone(),
        default_guest_cpus: run_opts.default_guest_cpus,
        default_guest_memory_mib: run_opts.default_guest_memory_mib,
    };

    // The VM factory config and artifact store are enough to create a test
    // context to pass to test cases and a set of fixtures.
    let ctx = TestContext {
        vm_factory: phd_framework::test_vm::factory::VmFactory::new(
            factory_config,
            &artifact_store,
        )
        .unwrap(),
    };
    let fixtures = TestFixtures::new(&run_opts, &artifact_store).unwrap();

    // Run the tests and print results.
    let execution_stats =
        execute::run_tests_with_ctx(ctx, fixtures, &process_args);
    if execution_stats.failed_test_cases.len() != 0 {
        println!("\nfailures:");
        for tc in execution_stats.failed_test_cases {
            println!("    {}", tc.fully_qualified_name());
        }
        print!("\n");
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
}

fn list_tests(runner_config: ProcessArgs) {
    println!("Tests enabled after applying filters:\n");

    let mut count = 0;
    for tc in
        phd_tests::phd_testcase::all_test_cases().into_iter().filter(|tc| {
            let filt = filter::TestCaseFilter {
                must_include: &runner_config.include_filter,
                must_exclude: &runner_config.exclude_filter,
            };
            filt.check(&tc.fully_qualified_name())
        })
    {
        println!("    {}", tc.fully_qualified_name());
        count += 1
    }
    println!("\n{} tests selected", count);
}
