// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;
use std::time::{Duration, Instant};

use phd_tests::phd_testcase::{Framework, TestCase, TestOutcome};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::config::RunOptions;
use crate::fixtures::TestFixtures;

/// Statistics returned after executing a set of tests.
pub struct ExecutionStats {
    /// The number of tests that passed.
    pub tests_passed: u32,

    /// The number of tests that failed.
    pub tests_failed: u32,

    /// The number of tests that marked themselves as skipped.
    pub tests_skipped: u32,

    /// The number of tests that the runner decided not to run (e.g. because of
    /// a failure in a fixture).
    pub tests_not_run: u32,

    /// The total time spent running tests and fixtures. This spans the time
    /// from just before the first test setup fixture runs to the time just
    /// after the last fixture finishes.
    pub duration: Duration,

    /// A collection of test cases that returned a failed result.
    pub failed_test_cases: Vec<&'static TestCase>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Ran(TestOutcome),
    NotRun,
}

struct Execution {
    tc: &'static TestCase,
    status: Status,
}

/// Executes a set of tests using the supplied test context.
pub async fn run_tests_with_ctx(
    ctx: &Arc<Framework>,
    mut fixtures: TestFixtures,
    run_opts: &RunOptions,
) -> ExecutionStats {
    let mut executions = Vec::new();

    for tc in phd_tests::phd_testcase::filtered_test_cases(
        &run_opts.include_filter,
        &run_opts.exclude_filter,
    ) {
        executions.push(Execution { tc, status: Status::NotRun });
    }

    let mut stats = ExecutionStats {
        tests_passed: 0,
        tests_failed: 0,
        tests_skipped: 0,
        tests_not_run: executions.len() as u32,
        duration: Duration::default(),
        failed_test_cases: Vec::new(),
    };

    if executions.is_empty() {
        info!("No tests selected for execution");
        return stats;
    }

    fixtures.execution_setup().unwrap();
    let sigint_rx = set_sigint_handler();
    info!("Running {} test(s)", executions.len());
    let start_time = Instant::now();
    for execution in &mut executions {
        if *sigint_rx.borrow() {
            info!("Test run interrupted by SIGINT");
            break;
        }

        info!("Starting test {}", execution.tc.fully_qualified_name());

        // Failure to run a setup fixture is fatal to the rest of the run, but
        // it's still possible to report results, so return gracefully instead
        // of panicking.
        if let Err(e) = fixtures.test_setup() {
            error!("Error running test setup fixture: {}", e);
            break;
        }

        stats.tests_not_run -= 1;
        let test_ctx = ctx.clone();
        let tc = execution.tc;
        let mut sigint_rx_task = sigint_rx.clone();
        let test_outcome = tokio::spawn(async move {
            tokio::select! {
                // Ensure interrupt signals are always handled instead of
                // continuing to run the test.
                biased;
                result = sigint_rx_task.changed() => {
                    assert!(
                        result.is_ok(),
                        "SIGINT channel shouldn't drop while tests are running"
                    );

                    TestOutcome::Failed(
                        Some("test interrupted by SIGINT".to_string())
                    )
                }
                outcome = tc.run(test_ctx.as_ref()) => outcome
            }
        })
        .await
        .unwrap_or_else(|_| {
            TestOutcome::Failed(Some(
                "test task panicked, see test logs".to_string(),
            ))
        });

        info!(
            "test {} ... {}{}",
            execution.tc.fully_qualified_name(),
            match test_outcome {
                TestOutcome::Passed => "ok",
                TestOutcome::Failed(_) => "FAILED: ",
                TestOutcome::Skipped(_) => "skipped: ",
            },
            match &test_outcome {
                TestOutcome::Failed(Some(s))
                | TestOutcome::Skipped(Some(s)) => s,
                TestOutcome::Failed(None) | TestOutcome::Skipped(None) =>
                    "[no message]",
                _ => "",
            }
        );

        match test_outcome {
            TestOutcome::Passed => stats.tests_passed += 1,
            TestOutcome::Failed(_) => {
                stats.tests_failed += 1;
                stats.failed_test_cases.push(execution.tc);
            }
            TestOutcome::Skipped(_) => stats.tests_skipped += 1,
        }

        execution.status = Status::Ran(test_outcome);
        if let Err(e) = fixtures.test_cleanup().await {
            error!("Error running cleanup fixture: {}", e);
            break;
        }
    }
    stats.duration = start_time.elapsed();

    fixtures.execution_cleanup().unwrap();

    stats
}

/// Sets a global handler for SIGINT and hands the resulting signal channel over
/// to a task that handles this signal. Returns a receiver to which the signal
/// handler task publishes `true` to the channel when SIGINT is received.
fn set_sigint_handler() -> watch::Receiver<bool> {
    let mut sigint =
        signal(SignalKind::interrupt()).expect("failed to set SIGINT handler");

    let (sigint_tx, sigint_rx) = watch::channel(false);
    tokio::spawn(async move {
        loop {
            sigint.recv().await;

            // If a signal was previously dispatched to the channel, exit
            // immediately with the customary SIGINT exit code (130 is 128 +
            // SIGINT). This allows users to interrupt tests even if they aren't
            // at an await point (at the cost of not having destructors run).
            if *sigint_tx.borrow() {
                error!(
                    "SIGINT received while shutting down, rudely terminating"
                );
                error!("some processes and resources may have been leaked!");
                std::process::exit(130);
            }

            warn!("SIGINT received, sending shutdown signal to tests");
            let _ = sigint_tx.send(true);
        }
    });

    sigint_rx
}
