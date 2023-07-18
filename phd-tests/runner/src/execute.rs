// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use phd_tests::phd_testcase::{TestCase, TestContext, TestOutcome};
use tracing::{error, info};

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

// The executor will install a global panic hook that allows a thread that
// panics to store a message for the executor to log after unwinding. A
// `RefCell` is safe here because this message is stored once per thread, and a
// thread running the panic hook is by definition not running the code that
// handles a caught panic.
thread_local! {
    static PANIC_MSG: RefCell<Option<String>> = RefCell::new(None);
}

/// Executes a set of tests using the supplied test context.
pub fn run_tests_with_ctx(
    ctx: &TestContext,
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

    std::panic::set_hook(Box::new(|info| {
        PANIC_MSG.with(|val| {
            let backtrace = backtrace::Backtrace::new();
            let msg = format!("{}\n    backtrace:\n{:#?}", info, backtrace);
            eprintln!("Caught a panic: {}", msg);
            *val.borrow_mut() = Some(msg);
        });
    }));

    info!("Running {} test(s)", executions.len());
    let start_time = Instant::now();
    'exec_loop: for execution in &mut executions {
        info!("Starting test {}", execution.tc.fully_qualified_name());

        // Failure to run a setup fixture is fatal to the rest of the run, but
        // it's still possible to report results, so return gracefully instead
        // of panicking.
        if let Err(e) = fixtures.test_setup() {
            error!("Error running test setup fixture: {}", e);
            break 'exec_loop;
        }

        stats.tests_not_run -= 1;
        let test_outcome = std::panic::catch_unwind(|| execution.tc.run(ctx))
            .unwrap_or_else(|_| {
                PANIC_MSG.with(|val| TestOutcome::Failed(val.take()))
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
        if let Err(e) = fixtures.test_cleanup() {
            error!("Error running cleanup fixture: {}", e);
            break 'exec_loop;
        }
    }
    stats.duration = start_time.elapsed();

    fixtures.execution_cleanup().unwrap();

    stats
}
