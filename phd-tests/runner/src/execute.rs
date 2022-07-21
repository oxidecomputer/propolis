use std::cell::RefCell;
use std::time::{Duration, Instant};

use phd_tests::phd_testcase::{TestCase, TestContext, TestOutcome};
use tracing::{error, info};

use crate::fixtures::TestFixtures;

pub struct ExecutionStats {
    pub tests_passed: u32,
    pub tests_failed: u32,
    pub tests_skipped: u32,
    pub tests_not_run: u32,
    pub duration: Duration,

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

thread_local! {
    static PANIC_MSG: RefCell<Option<String>> = RefCell::new(None);
}

pub fn run_tests_with_ctx<'fix>(
    ctx: TestContext,
    mut fixtures: TestFixtures,
) -> ExecutionStats {
    let mut executions = Vec::new();
    for tc in phd_tests::phd_testcase::all_test_cases() {
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

    std::panic::set_hook(Box::new(|info| {
        PANIC_MSG.with(|val| {
            let backtrace = backtrace::Backtrace::new();
            let msg = format!("{}\n    backtrace:\n{:#?}", info, backtrace);
            eprintln!("Caught a panic: {}", msg);
            *val.borrow_mut() = Some(msg);
        });
    }));

    fixtures.execution_setup().unwrap();

    info!("Running {} tests", executions.len());
    let start_time = Instant::now();
    'exec_loop: for execution in &mut executions {
        info!("Starting test {}", execution.tc.fully_qualified_name());
        if let Err(e) = fixtures.test_setup() {
            error!("Error running test setup fixture: {}", e);
            break 'exec_loop;
        }

        stats.tests_not_run -= 1;
        let test_outcome = std::panic::catch_unwind(|| execution.tc.run(&ctx))
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
