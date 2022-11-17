use phd_testcase::{
    phd_framework::{
        disk::{DiskBackend, DiskSource},
        test_vm::vm_config::DiskInterface,
    },
    *,
};

#[phd_testcase]
fn nproc_test(ctx: &TestContext) {
    let mut vm = ctx
        .vm_factory
        .new_vm("nproc_test", ctx.default_vm_config().set_cpus(6))?;
    vm.launch()?;
    vm.wait_to_boot()?;

    let nproc = vm.run_shell_command("nproc")?;
    assert_eq!(nproc.parse::<u8>().unwrap(), 6);
}

#[phd_testcase]
fn instance_spec_get_test(ctx: &TestContext) {
    let disk = ctx.disk_factory.create_disk(
        DiskSource::Artifact(&ctx.default_guest_image_artifact),
        DiskBackend::File,
    )?;

    let config = ctx
        .deviceless_vm_config()
        .set_cpus(4)
        .set_memory_mib(3072)
        .set_boot_disk(disk, 4, DiskInterface::Nvme);
    let mut vm = ctx.vm_factory.new_vm("instance_spec_test", config)?;
    vm.launch()?;

    let spec_get_response = vm.get_spec()?;
    let spec = spec_get_response.spec;
    assert_eq!(spec.devices.board.cpus, 4);
    assert_eq!(spec.devices.board.memory_mb, 3072);
}
