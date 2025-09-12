// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

use phd_framework::{
    artifacts, lifecycle::Action, test_vm::MigrationTimeout, TestVm,
};
use phd_testcase::*;
use propolis_client::types::{InstanceState, MigrationState};
use tracing::info;
use uuid::Uuid;

#[phd_testcase]
async fn smoke_test(ctx: &Framework) {
    run_smoke_test(ctx, ctx.spawn_default_vm("migration_smoke").await?).await?;
}

#[phd_testcase]
async fn serial_history(ctx: &Framework) {
    run_serial_history_test(
        ctx,
        ctx.spawn_default_vm("migration_serial_history").await?,
    )
    .await?;
}

/// Tests for migrating from a "migration base" Propolis revision (e.g. the
/// latest commit to the `master` git branch) to the revision under test.
mod from_base {
    use super::*;

    #[phd_testcase]
    async fn can_migrate_from_base(ctx: &Framework) {
        run_smoke_test(ctx, spawn_base_vm(ctx, "migration_from_base").await?)
            .await?;
    }

    #[phd_testcase]
    async fn serial_history(ctx: &Framework) {
        run_serial_history_test(
            ctx,
            spawn_base_vm(ctx, "migration_serial_history_base").await?,
        )
        .await?;
    }

    // Tests migrating from the "migration base" propolis artifact to the Propolis
    // version under test, back to "base", and back to the version under
    // test.
    #[phd_testcase]
    async fn migration_from_base_and_back(ctx: &Framework) {
        let mut source =
            spawn_base_vm(ctx, "migration_from_base_and_back").await?;
        source.launch().await?;
        source.wait_to_boot().await?;
        // `ls` with no results exits non-zero, so expect an error here.
        let lsout = source
            .run_shell_command("ls foo.bar 2> /dev/null")
            .check_err()
            .await?;
        assert_eq!(lsout, "");

        // create an empty file on the source VM.
        source.run_shell_command("touch ./foo.bar").await?;
        source.run_shell_command("sync ./foo.bar").await?;

        ctx.lifecycle_test(
            source,
            &[
                Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT),
                Action::MigrateToPropolis(artifacts::BASE_PROPOLIS_ARTIFACT),
                Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT),
            ],
            |target: &TestVm| {
                Box::pin(async {
                    // the file should still exist on the target VM after migration.
                    let lsout = target
                        .run_shell_command("ls foo.bar")
                        .ignore_status()
                        .await
                        .expect("can try to run `ls foo.bar`");
                    assert_eq!(lsout, "foo.bar");
                })
            },
        )
        .await?;
    }

    async fn spawn_base_vm(ctx: &Framework, name: &str) -> Result<TestVm> {
        if !ctx.migration_base_enabled() {
            phd_skip!("No 'migration base' Propolis revision available");
        }

        let mut env = ctx.environment_builder();
        env.propolis(artifacts::BASE_PROPOLIS_ARTIFACT);
        let mut cfg = ctx.vm_config_builder(name);
        // TODO: not strictly necessary, but as of #756 PHD began adding a
        // `boot_settings` key by default to new instances. This is not
        // understood by older Propolis, so migration tests would fail because
        // the test changed, rather than a migration issue.
        //
        // At some point after landing #756, stop clearing the boot order,
        // because a newer base Propolis will understand `boot_settings` just
        // fine.
        cfg.clear_boot_order();
        ctx.spawn_vm(&cfg, Some(&env)).await
    }
}

/// Tests for migrations while a process is running on the guest.
mod running_process {
    use super::*;

    #[phd_testcase]
    async fn migrate_running_process(ctx: &Framework) {
        let mut source =
            ctx.spawn_default_vm("migrate_running_process_source").await?;
        let mut target = ctx
            .spawn_successor_vm("migrate_running_process_target", &source, None)
            .await?;

        source.launch().await?;
        source.wait_to_boot().await?;

        mk_dirt(&source).await?;

        target
            .migrate_from(&source, Uuid::new_v4(), MigrationTimeout::default())
            .await?;

        check_dirt(&target).await?;
    }

    #[phd_testcase]
    async fn import_failure(ctx: &Framework) {
        let mut cfg = ctx.vm_config_builder(
            "migrate_running_process::import_failure_source",
        );
        // Ensure the migration failure device is present in the VM config for
        // the source as well as the target, so that the source will offer the
        // device.
        cfg.fail_migration_imports(0);
        let mut source = ctx.spawn_vm(&cfg, None).await?;

        let mut target1 = {
            // Configure the target to fail when it imports the migration
            // failure device.
            cfg.named("migrate_running_process::import_failure_target1")
                .fail_migration_imports(1);

            // N.B. that we don't use `spawn_successor_vm` here, because we must
            // add the `fail_migration_imports` option to the new VM's
            // `VmConfig`. Instead, we use `spawn_vm`, and pass the source VM's
            // environment to ensure it's inherited.
            ctx.spawn_vm(&cfg, Some(&source.environment_spec())).await?
        };

        let mut target2 = ctx
            .spawn_successor_vm(
                "migrate_running_process::import_failure_target2",
                &source,
                None,
            )
            .await?;

        source.launch().await?;
        source.wait_to_boot().await?;

        mk_dirt(&source).await?;

        // first migration should fail.
        let error = target1
            .migrate_from(&source, Uuid::new_v4(), MigrationTimeout::default())
            .await
            .unwrap_err();
        info!(%error, "first migration failed as expected");

        // Also verify that the target reports that it failed.
        let target_migration_state = target1
            .get_migration_state()
            .await?
            .migration_in
            .expect("target should have a migration-in status")
            .state;
        assert_eq!(target_migration_state, MigrationState::Error);

        // Wait for the source to report that it has resumed before requesting
        // another migration.
        source
            .wait_for_state(
                InstanceState::Running,
                std::time::Duration::from_secs(5),
            )
            .await?;

        // try again. this time, it should work!
        target2
            .migrate_from(&source, Uuid::new_v4(), MigrationTimeout::default())
            .await?;

        check_dirt(&target2).await?;
    }

    #[phd_testcase]
    async fn export_failure(ctx: &Framework) {
        let mut source = {
            let mut cfg = ctx.vm_config_builder(
                "migrate_running_process::export_failure_source",
            );
            cfg.fail_migration_exports(1);
            ctx.spawn_vm(&cfg, None).await?
        };
        let mut target1 = ctx
            .spawn_successor_vm(
                "migrate_running_process::export_failure_target1",
                &source,
                None,
            )
            .await?;
        let mut target2 = ctx
            .spawn_successor_vm(
                "migrate_running_process::export_failure_target2",
                &source,
                None,
            )
            .await?;

        source.launch().await?;
        source.wait_to_boot().await?;

        mk_dirt(&source).await?;

        // first migration should fail.
        let error = target1
            .migrate_from(&source, Uuid::new_v4(), MigrationTimeout::default())
            .await
            .unwrap_err();
        info!(%error, "first migration failed as expected");

        // Also verify that the target reports that it failed.
        let target_migration_state = target1
            .get_migration_state()
            .await?
            .migration_in
            .expect("target should have a migration-in status")
            .state;
        assert_eq!(target_migration_state, MigrationState::Error);

        // Wait for the source to report that it has resumed before requesting
        // another migration.
        source
            .wait_for_state(
                InstanceState::Running,
                std::time::Duration::from_secs(5),
            )
            .await?;

        // try again. this time, it should work!
        target2
            .migrate_from(&source, Uuid::new_v4(), MigrationTimeout::default())
            .await?;

        check_dirt(&target2).await?;
    }

    /// Starts a process on the guest VM which stores a bunch of strings in
    /// memory and then suspends itself, waiting to be brought to the
    /// foreground. Once the process is resumed, it checks the contents of the
    /// string to ensure that they're still valid.
    ///
    /// Resuming this process after migration allows us to check that the
    /// guest's memory was migrated correctly.
    async fn mk_dirt(vm: &TestVm) -> phd_testcase::Result<()> {
        vm.run_shell_command(concat!(
            "cat >dirt.sh <<'EOF'\n",
            include_str!("../testdata/dirt.sh"),
            "\nEOF"
        ))
        .await?;
        vm.run_shell_command("chmod +x dirt.sh").await?;
        // When dirt.sh suspends itself, the parent shell will report a non-zero
        // status (one example is 148: 128 + SIGTSTP aka 20 on Linux).
        let run_dirt = vm.run_shell_command("./dirt.sh").check_err().await?;
        assert!(run_dirt.contains("made dirt"), "dirt.sh failed: {run_dirt:?}");
        assert!(
            run_dirt.contains("Stopped"),
            "dirt.sh didn't suspend: {run_dirt:?}"
        );

        Ok(())
    }

    async fn check_dirt(vm: &TestVm) -> phd_testcase::Result<()> {
        let output = vm.run_shell_command("fg").await?;
        assert!(output.contains("all good"), "dirt.sh failed: {output:?}");
        Ok(())
    }
}

#[phd_testcase]
async fn multiple_migrations(ctx: &Framework) {
    let mut vm0 = ctx.spawn_default_vm("multiple_migrations_0").await?;
    let mut vm1 =
        ctx.spawn_successor_vm("multiple_migrations_1", &vm0, None).await?;
    let mut vm2 =
        ctx.spawn_successor_vm("multiple_migrations_2", &vm1, None).await?;

    vm0.launch().await?;
    vm0.wait_to_boot().await?;
    vm1.migrate_from(&vm0, Uuid::new_v4(), MigrationTimeout::default()).await?;
    assert_eq!(vm1.run_shell_command("echo Hello world").await?, "Hello world");
    vm2.migrate_from(&vm1, Uuid::new_v4(), MigrationTimeout::default()).await?;
    assert_eq!(
        vm2.run_shell_command("echo I have migrated!").await?,
        "I have migrated!"
    );
}

async fn run_smoke_test(ctx: &Framework, mut source: TestVm) -> Result<()> {
    source.launch().await?;
    source.wait_to_boot().await?;
    let lsout =
        source.run_shell_command("ls foo.bar 2> /dev/null").check_err().await?;
    assert_eq!(lsout, "");

    // create an empty file on the source VM.
    source.run_shell_command("touch ./foo.bar").await?;
    source.run_shell_command("sync ./foo.bar").await?;

    ctx.lifecycle_test(
        source,
        &[Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT)],
        |target: &TestVm| {
            Box::pin(async {
                // the file should still exist on the target VM after migration.
                let lsout = target
                    .run_shell_command("ls foo.bar")
                    .ignore_status()
                    .await
                    .expect("can try to run `ls foo.bar`");
                assert_eq!(lsout, "foo.bar");
            })
        },
    )
    .await
}

async fn run_serial_history_test(
    ctx: &Framework,
    mut source: TestVm,
) -> Result<()> {
    source.launch().await?;
    source.wait_to_boot().await?;

    let out =
        source.run_shell_command("echo hello from the source VM!").await?;
    assert_eq!(out, "hello from the source VM!");

    let serial_hist_pre = source.get_serial_console_history(0).await?;
    assert!(!serial_hist_pre.data.is_empty());

    ctx.lifecycle_test(
        source,
        &[Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT)],
        move |target| {
            let serial_hist_pre = serial_hist_pre.clone();
            Box::pin(async move {
                let serial_hist_post =
                    target.get_serial_console_history(0).await.expect(
                        "should get serial console history from the target",
                    );
                assert_eq!(
                    serial_hist_pre.data,
                    serial_hist_post.data[..serial_hist_pre.data.len()]
                );
                assert!(
                    serial_hist_pre.last_byte_offset
                        <= serial_hist_post.last_byte_offset
                );
            })
        },
    )
    .await
}

#[phd_testcase]
async fn migration_ensures_instance_metadata(ctx: &Framework) {
    // Create a source instance, and fetch the instance metadata its metrics are
    // generated with.
    let mut source = ctx
        .spawn_default_vm("migration_ensures_instance_metadata_source")
        .await?;
    let mut target = ctx
        .spawn_successor_vm(
            "migration_ensures_instance_metadata_target",
            &source,
            None,
        )
        .await?;
    source.launch().await?;
    source.wait_to_boot().await?;
    let expected_metadata = source.vm_spec().metadata;
    let source_metadata = source.get_spec().await?.properties.metadata;
    assert_eq!(
        expected_metadata, source_metadata,
        "Source instance was not populated with the correct instance metadata"
    );

    // Migrate the instance to a new server, and refetch the metadata.
    target
        .migrate_from(&source, Uuid::new_v4(), MigrationTimeout::default())
        .await?;
    let expected_metadata = target.vm_spec().metadata;
    let target_metadata = target.get_spec().await?.properties.metadata;
    assert_eq!(
        expected_metadata, target_metadata,
        "Target instance was not populated with the correct instance metadata"
    );

    // Check that the source / target sled identifiers are different.
    assert_ne!(
        source_metadata.sled_serial, target_metadata.sled_serial,
        "Source and target serial numbers should be different"
    );
    assert_ne!(
        source_metadata.sled_id, target_metadata.sled_id,
        "Source and target UUIDs should be different"
    );
}

#[phd_testcase]
async fn vm_reaches_destroyed_after_migration_out(ctx: &Framework) {
    let mut source = ctx
        .spawn_default_vm("vm_reaches_destroyed_after_migration_out_source")
        .await?;

    let mut target = ctx
        .spawn_successor_vm(
            "vm_reaches_destroyed_after_migration_out_target",
            &source,
            None,
        )
        .await?;

    source.launch().await?;
    source.wait_to_boot().await?;
    target
        .migrate_from(&source, Uuid::new_v4(), MigrationTimeout::default())
        .await?;

    source
        .wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))
        .await?;
}
