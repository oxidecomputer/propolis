use phd_testcase::*;

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
fn multiple_vms_test(ctx: &TestContext) {
    let mut vms = (0..5)
        .into_iter()
        .map(|i| {
            let name = format!("multiple_vms_test_vm{}", i);
            ctx.vm_factory.new_vm(&name, ctx.default_vm_config())
        })
        .collect::<Result<Vec<_>, _>>()?;

    for vm in &mut vms {
        vm.launch()?;
    }

    for vm in &vms {
        vm.wait_to_boot()?;
    }
}
