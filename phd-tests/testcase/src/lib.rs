// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub use anyhow::{Context, Result};
pub use inventory::submit as inventory_submit;
pub use phd_framework;
pub use phd_testcase_macros::*;
use thiserror::Error;

pub use phd_framework::Framework;
pub use phd_framework::FrameworkParameters;

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
    pub f: fn(&Framework) -> futures::future::BoxFuture<'_, TestOutcome>,
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
}

#[allow(dead_code)]
impl TestCase {
    /// Constructs a new [`TestCase`].
    pub const fn new(
        module_path: &'static str,
        name: &'static str,
        function: TestFunction,
    ) -> Self {
        Self { module_path, name, function }
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
    pub async fn run(&self, ctx: &Framework) -> TestOutcome {
        (self.function.f)(ctx).await
    }
}

inventory::collect!(TestCase);

pub fn all_test_cases() -> impl Iterator<Item = &'static TestCase> {
    inventory::iter::<TestCase>.into_iter()
}

/// Returns an iterator over the subset of tests for which (a) the fully
/// qualified name of the test includes every string in `must_include`, and (b)
/// the fully qualified name does not include any strings in `must_exclude`.
pub fn filtered_test_cases<'rule>(
    must_include: &'rule [String],
    must_exclude: &'rule [String],
) -> impl Iterator<Item = &'static TestCase> + 'rule {
    inventory::iter::<TestCase>.into_iter().filter(|tc| {
        must_include.iter().all(|inc| tc.fully_qualified_name().contains(inc))
            && must_exclude
                .iter()
                .all(|exc| !tc.fully_qualified_name().contains(exc))
    })
}
