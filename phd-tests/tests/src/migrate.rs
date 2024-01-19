// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

use phd_framework::{artifacts, lifecycle::Action, TestVm};
use phd_testcase::*;
use propolis_client::types::MigrationState;
use uuid::Uuid;

#[phd_testcase]
fn smoke_test(ctx: &Framework) {
    let src = ctx.spawn_default_vm("migration_smoke_source")?;
    run_smoke_test(ctx, src, artifacts::DEFAULT_PROPOLIS_ARTIFACT)?;
}

#[phd_testcase]
fn serial_history(ctx: &Framework) {
    let mut source = ctx.spawn_default_vm("migration_serial_history_source")?;

    source.launch()?;
    source.wait_to_boot()?;

    let out = source.run_shell_command("echo hello from the source VM!")?;
    assert_eq!(out, "hello from the source VM!");

    let serial_hist_pre = source.get_serial_console_history(0)?;
    assert!(!serial_hist_pre.data.is_empty());

    ctx.lifecycle_test(
        source,
        &[Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT)],
        |target| {
            let serial_hist_post = target.get_serial_console_history(0).expect(
                "getting serial console history from the second VM should work",
            );
            assert_eq!(
                serial_hist_pre.data,
                serial_hist_post.data[..serial_hist_pre.data.len()]
            );
            assert!(
                serial_hist_pre.last_byte_offset
                    <= serial_hist_post.last_byte_offset
            );
        },
    )?;
}

#[phd_testcase]
fn can_migrate_from_current(ctx: &Framework) {
    if !ctx.current_propolis_enabled() {
        phd_skip!("No 'current' Propolis revision available");
    }

    let src = ctx.spawn_default_vm("migration_smoke_source")?;
    run_smoke_test(ctx, src, artifacts::CURRENT_PROPOLIS_ARTIFACT)?;
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

fn run_smoke_test(
    ctx: &Framework,
    mut source: TestVm,
    target_propolis: &str,
) -> Result<()> {
    source.launch()?;
    source.wait_to_boot()?;
    let lsout = source.run_shell_command("ls foo.bar 2> /dev/null")?;
    assert_eq!(lsout, "");

    // create an empty file on the source VM.
    source.run_shell_command("touch ./foo.bar")?;
    source.run_shell_command("sync ./foo.bar")?;

    ctx.lifecycle_test(
        source,
        &[Action::MigrateToPropolis(target_propolis)],
        |target: &TestVm| {
            // the file should still exist on the target VM after migration.
            let lsout = target
                .run_shell_command("ls foo.bar")
                .expect("`ls foo.bar` should succeed after migration");
            assert_eq!(lsout, "foo.bar");
        },
    )
}
