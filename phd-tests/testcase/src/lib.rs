// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub use anyhow::{Context, Result};
pub use phd_framework;
pub use phd_testcase_macros::*;
use thiserror::Error;

pub use phd_framework::Framework;
pub use phd_framework::FrameworkParameters;
pub use phd_framework::TestCtx;

#[derive(Debug, Error)]
pub enum TestSkippedError {
    #[error("Test skipped: {0:?}")]
    TestSkipped(Option<String>),
}

/// The outcome from executing a specific test case.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TestOutcome {
    /// The test passed.
    Passed,

    /// The test failed.
    Failed(Option<String>),

    /// The test chose to be skipped, i.e. it detected a parameter or condition
    /// that makes it impossible to execute the test or to meaningfully provide
    /// a pass/fail outcome. The payload is an optional message.
    Skipped(Option<String>),
}

/// A wrapper for test functions. This is needed to allow [`TestCase`] to have a
/// `const` constructor for the inventory crate.
pub struct TestFunction {
    pub f: fn(&TestCtx) -> futures::future::BoxFuture<'_, TestOutcome>,
}

/// A description of a single test case.
pub struct TestCase {
    /// The path to the module containing the test case. This is generally
    /// derived from the `module_path!()` macro, which the `#[phd_testcase]`
    /// attribute macro uses when constructing the test case's inventory entry.
    pub(crate) module_path: &'static str,

    /// The name of this test case, which is generally its function name.
    pub(crate) name: &'static str,

    /// The test function to execute to run this test.
    pub(crate) function: TestFunction,

    /// A function used to check if a test's skip or pass was actually expected
    /// for the given `TestCtx`.
    pub(crate) check_skip_fn: Option<fn(&TestCtx) -> bool>,
}

#[allow(dead_code)]
impl TestCase {
    /// Constructs a new [`TestCase`].
    pub const fn new(
        module_path: &'static str,
        name: &'static str,
        function: TestFunction,
        check_skip_fn: Option<fn(&TestCtx) -> bool>,
    ) -> Self {
        Self { module_path, name, function, check_skip_fn }
    }

    /// Returns the test case's fully qualified name, i.e. `module_path::name`.
    pub fn fully_qualified_name(&self) -> String {
        format!("{}::{}", self.module_path, self.name)
    }

    /// Returns the test case's name.
    pub fn name(&self) -> &str {
        self.name
    }

    /// Runs the test case's body with the supplied test context and returns its
    /// outcome.
    pub async fn run(&self, ctx: &TestCtx) -> TestOutcome {
        let outcome = (self.function.f)(ctx).await;
        match (outcome, self.check_skip_fn) {
            (TestOutcome::Passed, check_skip_fn) => {
                let wanted_skip = if let Some(check_skip_fn) = check_skip_fn {
                    check_skip_fn(ctx)
                } else {
                    false
                };

                if wanted_skip {
                    return TestOutcome::Failed(Some(
                        "test passed but expected skip?".to_string(),
                    ));
                }

                TestOutcome::Passed
            }
            (TestOutcome::Failed(skip_msg), _) => TestOutcome::Failed(skip_msg),
            (TestOutcome::Skipped(skip_msg), None) => {
                let fail_msg = match skip_msg {
                    Some(msg) => {
                        format!("skipped without check_skip attribute: {msg}")
                    }
                    None => "skipped without check_skip attribute".to_string(),
                };
                TestOutcome::Failed(Some(fail_msg))
            }
            (TestOutcome::Skipped(skip_msg), Some(check_skip_fn)) => {
                if check_skip_fn(ctx) {
                    return TestOutcome::Skipped(skip_msg);
                }

                let fail_msg = match skip_msg {
                    Some(msg) => {
                        format!("skipped but did not expect skip: {msg}")
                    }
                    None => "skipped but did not expect skip".to_string(),
                };

                TestOutcome::Failed(Some(fail_msg))
            }
        }
    }
}

#[linkme::distributed_slice]
pub static TEST_CASES: [TestCase];

pub fn all_test_cases() -> impl Iterator<Item = &'static TestCase> {
    TEST_CASES.into_iter()
}

/// Returns an iterator over the subset of tests for which (a) the fully
/// qualified name of the test includes every string in `must_include`, and (b)
/// the fully qualified name does not include any strings in `must_exclude`.
pub fn filtered_test_cases<'rule>(
    must_include: &'rule [String],
    must_exclude: &'rule [String],
) -> impl Iterator<Item = &'static TestCase> + 'rule {
    TEST_CASES.into_iter().filter(|tc| {
        must_include.iter().all(|inc| tc.fully_qualified_name().contains(inc))
            && must_exclude
                .iter()
                .all(|exc| !tc.fully_qualified_name().contains(exc))
    })
}
