// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

use phd_testcase::*;
use propolis_client::types::MigrationState;
use uuid::Uuid;

#[phd_testcase]
fn smoke_test(ctx: &Framework) {
    let mut source = ctx.spawn_default_vm("migration_smoke_source")?;

    source.launch()?;
    source.wait_to_boot()?;
    let lsout = source.run_shell_command("ls foo.bar 2> /dev/null")?;
    assert_eq!(lsout, "");
    source.run_shell_command("touch ./foo.bar")?;
    source.run_shell_command("sync ./foo.bar")?;

    let mut target =
        ctx.spawn_successor_vm("migration_smoke_target", &source, None)?;

    let serial_hist_pre = source.get_serial_console_history(0)?;
    assert!(!serial_hist_pre.data.is_empty());

    let migration_id = Uuid::new_v4();
    target.migrate_from(&source, migration_id, Duration::from_secs(60))?;

    // Explicitly check migration status on both the source and target to make
    // sure it is available even after migration has finished.
    let src_migration_state = source.get_migration_state(migration_id)?;
    assert_eq!(src_migration_state, MigrationState::Finish);
    let target_migration_state = target.get_migration_state(migration_id)?;
    assert_eq!(target_migration_state, MigrationState::Finish);

    let serial_hist_post = target.get_serial_console_history(0)?;
    assert_eq!(
        serial_hist_pre.data,
        serial_hist_post.data[..serial_hist_pre.data.len()]
    );
    assert!(
        serial_hist_pre.last_byte_offset <= serial_hist_post.last_byte_offset
    );

    let lsout = target.run_shell_command("ls foo.bar")?;
    assert_eq!(lsout, "foo.bar");
}

#[phd_testcase]
fn incompatible_vms(ctx: &Framework) {
    let mut builders = vec![
        ctx.vm_config_builder("migration_incompatible_target_1"),
        ctx.vm_config_builder("migration_incompatible_target_2"),
    ];

    builders[0].cpus(8);
    builders[1].memory_mib(1024);

    for (i, cfg) in builders.into_iter().enumerate() {
        let mut source = ctx.spawn_vm(
            ctx.vm_config_builder(&format!(
                "migration_incompatible_source_{}",
                i
            ))
            .cpus(4)
            .memory_mib(512),
            None,
        )?;

        source.launch()?;
        let mut target = ctx.spawn_vm(&cfg, None)?;

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
fn multiple_migrations(ctx: &Framework) {
    let mut vm0 = ctx.spawn_default_vm("multiple_migrations_0")?;
    let mut vm1 =
        ctx.spawn_successor_vm("multiple_migrations_1", &vm0, None)?;
    let mut vm2 =
        ctx.spawn_successor_vm("multiple_migrations_2", &vm1, None)?;

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
