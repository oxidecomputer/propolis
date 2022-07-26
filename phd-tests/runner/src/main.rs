mod config;
mod execute;
mod fixtures;
pub(crate) mod zfs;

use phd_framework::artifacts::ArtifactStore;
use phd_tests::phd_testcase::TestContext;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Registry};

use crate::fixtures::TestFixtures;

fn main() {
    // Set up a tracing subscriber.
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into());
    let stdout_log = tracing_subscriber::fmt::layer();
    let subscriber =
        Registry::default().with(filter.from_env_lossy()).with(stdout_log);
    tracing::subscriber::set_global_default(subscriber).unwrap();

    // Set up global state: the command-line config and the artifact store.
    let runner_config = config::Config::get();
    let artifact_store =
        ArtifactStore::from_file(&runner_config.artifact_toml_path).unwrap();

    // Convert the command-line config and artifact store into a VM factory
    // definition.
    let mut config_toml_path = runner_config.tmp_directory.clone();
    config_toml_path.push("vm_config.toml");
    let factory_config = phd_framework::test_vm::factory::FactoryOptions {
        propolis_server_path: runner_config
            .propolis_server_cmd
            .to_string_lossy()
            .to_string(),
        tmp_directory: runner_config.tmp_directory.clone(),
        server_log_mode: runner_config.server_logging_mode,
        default_guest_image_artifact: runner_config
            .default_guest_artifact
            .clone(),
        default_bootrom_artifact: runner_config
            .default_bootrom_artifact
            .clone(),
        default_guest_cpus: runner_config.default_guest_cpus,
        default_guest_memory_mib: runner_config.default_guest_memory_mib,
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
    let fixtures = TestFixtures::new(&runner_config, &artifact_store).unwrap();

    // Run the tests and print results.
    let execution_stats = execute::run_tests_with_ctx(ctx, fixtures);
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
