use std::time::Duration;

use phd_testcase::*;

#[phd_testcase]
fn smoke_test(ctx: &TestContext) {
    let mut source = ctx
        .vm_factory
        .new_vm("migration_smoke_source", ctx.default_vm_config())?;

    source.launch()?;
    source.wait_to_boot()?;
    let lsout = source.run_shell_command("ls foo.bar 2> /dev/null")?;
    assert_eq!(lsout, "");
    source.run_shell_command("touch ./foo.bar")?;
    source.run_shell_command("sync ./foo.bar")?;

    let mut target = ctx
        .vm_factory
        .new_vm_from_cloned_config("migration_smoke_target", &source)?;

    target.migrate_from(&source, Duration::from_secs(60))?;
    let lsout = target.run_shell_command("ls foo.bar")?;
    assert_eq!(lsout, "foo.bar");
}

#[phd_testcase]
fn incompatible_vms(ctx: &TestContext) {
    let configs = vec![
        ctx.default_vm_config().set_cpus(8),
        ctx.default_vm_config().set_memory_mib(1024),
    ];

    for (i, cfg) in configs.into_iter().enumerate() {
        let mut source = ctx.vm_factory.new_vm(
            format!("migration_incompatible_source_{}", i).as_str(),
            ctx.default_vm_config().set_cpus(4).set_memory_mib(512),
        )?;

        source.launch()?;

        let mut target = ctx.vm_factory.new_vm(
            format!("migration_incompatible_target_{}", i).as_str(),
            cfg,
        )?;

        assert!(target.migrate_from(&source, Duration::from_secs(60)).is_err());
    }
}

#[phd_testcase]
fn multiple_migrations(ctx: &TestContext) {
    let mut vm0 = ctx
        .vm_factory
        .new_vm("multiple_migrations_0", ctx.default_vm_config())?;
    let mut vm1 = ctx
        .vm_factory
        .new_vm_from_cloned_config("multiple_migrations_1", &vm0)?;
    let mut vm2 = ctx
        .vm_factory
        .new_vm_from_cloned_config("multiple_migrations_2", &vm1)?;

    vm0.launch()?;
    vm0.wait_to_boot()?;
    vm1.migrate_from(&vm0, Duration::from_secs(60))?;
    assert_eq!(vm1.run_shell_command("echo Hello world")?, "Hello world");
    vm2.migrate_from(&vm1, Duration::from_secs(60))?;
    assert_eq!(
        vm2.run_shell_command("echo I have migrated!")?,
        "I have migrated!"
    );
}
