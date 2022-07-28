use phd_testcase::*;

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

#[phd_testcase]
fn multiple_vms_test(ctx: &TestContext) {
    let vms = (0..5)
        .into_iter()
        .map(|i| {
            let name = format!("vm{}", i);
            ctx.vm_factory.new_vm(&name, ctx.vm_factory.default_vm_config())
        })
        .collect::<Result<Vec<_>, _>>()?;

    for vm in &vms {
        vm.run()?;
    }

    for vm in &vms {
        vm.wait_to_boot()?;
    }
}
