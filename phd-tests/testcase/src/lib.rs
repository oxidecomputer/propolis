pub use anyhow::Result;
pub use inventory::submit as inventory_submit;
pub use phd_framework;
pub use phd_testcase_macros::phd_testcase;

use phd_framework::test_vm::factory::VmFactory;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TestOutcome {
    Passed,
    Failed(Option<String>),
    Skipped(Option<String>),
}

pub struct TestContext {
    pub vm_factory: VmFactory,
}

pub struct TestFunction {
    pub f: fn(&TestContext) -> TestOutcome,
}

pub struct TestCase {
    pub(crate) module_path: &'static str,
    pub(crate) name: &'static str,
    pub(crate) function: TestFunction,
}

#[allow(dead_code)]
impl TestCase {
    pub const fn new(
        module_path: &'static str,
        name: &'static str,
        function: TestFunction,
    ) -> Self {
        Self { module_path, name, function }
    }

    pub fn fully_qualified_name(&self) -> String {
        format!("{}::{}", self.module_path, self.name)
    }

    pub fn name(&self) -> &str {
        self.name
    }

    pub fn run(&self, ctx: &TestContext) -> TestOutcome {
        (self.function.f)(ctx)
    }
}

inventory::collect!(TestCase);

pub fn all_test_cases() -> inventory::iter<TestCase> {
    inventory::iter::<TestCase>
}
