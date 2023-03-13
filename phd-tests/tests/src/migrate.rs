use std::time::Duration;

use phd_testcase::*;
use propolis_client::handmade::api::MigrationState;
use uuid::Uuid;

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

    let migration_id = Uuid::new_v4();
    target.migrate_from(&source, migration_id, Duration::from_secs(60))?;

    // Explicitly check migration status on both the source and target to make
    // sure it is available even after migration has finished.
    let src_migration_state = source.get_migration_state(migration_id)?;
    assert_eq!(src_migration_state, MigrationState::Finish);
    let target_migration_state = target.get_migration_state(migration_id)?;
    assert_eq!(target_migration_state, MigrationState::Finish);

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

        let migration_id = Uuid::new_v4();
        assert!(target
            .migrate_from(&source, migration_id, Duration::from_secs(60))
            .is_err());

        // Explicitly check migration status on both the source and target to
        // make sure it is available even after migration has finished.
        let src_migration_state = source.get_migration_state(migration_id)?;
        assert_eq!(src_migration_state, MigrationState::Error);
        let target_migration_state =
            target.get_migration_state(migration_id)?;
        assert_eq!(target_migration_state, MigrationState::Error);
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
    vm1.migrate_from(&vm0, Uuid::new_v4(), Duration::from_secs(60))?;
    assert_eq!(vm1.run_shell_command("echo Hello world")?, "Hello world");
    vm2.migrate_from(&vm1, Uuid::new_v4(), Duration::from_secs(60))?;
    assert_eq!(
        vm2.run_shell_command("echo I have migrated!")?,
        "I have migrated!"
    );
}
