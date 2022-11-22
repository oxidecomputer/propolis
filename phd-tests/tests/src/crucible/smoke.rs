use phd_testcase::{phd_framework::test_vm::vm_config::DiskInterface, *};

#[phd_testcase]
fn boot_test(ctx: &TestContext) {
    let disk = super::create_default_boot_disk(ctx, 10)?;
    let config = ctx.deviceless_vm_config().set_boot_disk(
        disk.clone(),
        4,
        DiskInterface::Nvme,
    );

    let mut vm = ctx.vm_factory.new_vm("crucible_boot_test", config)?;
    vm.launch()?;
    vm.wait_to_boot()?;
}
