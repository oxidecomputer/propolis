use phd_testcase::{phd_framework::guest_os::GuestOsKind, *};

#[phd_testcase]
fn uname_test(ctx: &TestContext) {
    let vm = ctx
        .vm_factory
        .new_vm("uname_test", ctx.vm_factory.default_vm_config())?;
    vm.run()?;
    vm.wait_to_boot()?;

    vm.run_shell_command("echo $SHELL")?;

    let uname = vm.run_shell_command("uname -r")?;
    assert_eq!(
        uname,
        match vm.guest_os_kind() {
            GuestOsKind::Alpine => "5.15.41-0-virt",
            GuestOsKind::Debian11NoCloud => "5.10.0-16-amd64",
        }
    );
}

#[phd_testcase]
fn nproc_test(ctx: &TestContext) {
    let vm = ctx
        .vm_factory
        .new_vm("nproc_test", ctx.vm_factory.default_vm_config().set_cpus(6))?;
    vm.run()?;
    vm.wait_to_boot()?;

    let nproc = vm.run_shell_command("nproc")?;
    assert_eq!(nproc.parse::<u8>().unwrap(), 6);
}
