// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_framework::{artifacts, lifecycle::Action};
use phd_testcase::*;
use propolis_client::instance_spec::InstanceSpecStatus;

#[phd_testcase]
async fn native_acpi_tables_in_spec(ctx: &Framework) {
    let mut vm = ctx.spawn_default_vm("native_acpi_tables_in_spec").await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let InstanceSpecStatus::Present(spec) = vm.get_spec().await?.spec else {
        panic!("instance should have a spec");
    };

    assert_eq!(spec.board.native_acpi_tables, Some(true));
}

#[phd_testcase]
async fn native_tables_preserved_on_migration(ctx: &Framework) {
    let mut source =
        ctx.spawn_default_vm("native_tables_migration_source").await?;

    source.launch().await?;
    source.wait_to_boot().await?;

    ctx.lifecycle_test(
        source,
        &[
            Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT),
            Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT),
        ],
        |vm| {
            Box::pin(async {
                let InstanceSpecStatus::Present(spec) =
                    vm.get_spec().await.unwrap().spec
                else {
                    panic!("should have a spec");
                };
                assert_eq!(spec.board.native_acpi_tables, Some(true));
            })
        },
    )
    .await?;
}

#[phd_testcase]
async fn ovmf_tables_preserved_on_migration(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("ovmf_tables_migration_source");
    cfg.native_acpi_tables(Some(false));

    let mut source = ctx.spawn_vm(&cfg, None).await?;

    source.launch().await?;
    source.wait_to_boot().await?;

    ctx.lifecycle_test(
        source,
        &[
            Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT),
            Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT),
        ],
        |vm| {
            Box::pin(async {
                let InstanceSpecStatus::Present(spec) =
                    vm.get_spec().await.unwrap().spec
                else {
                    panic!("should have a spec");
                };
                assert_eq!(spec.board.native_acpi_tables, Some(false));
            })
        },
    )
    .await?;
}

mod from_base {
    use super::*;

    #[phd_testcase]
    async fn ovmf_tables_preserved_through_migrations(ctx: &Framework) {
        if !ctx.migration_base_enabled() {
            phd_skip!("No 'migration base' Propolis revision available");
        }

        let mut env = ctx.environment_builder();
        env.propolis(artifacts::BASE_PROPOLIS_ARTIFACT);
        let mut cfg = ctx.vm_config_builder("ovmf_tables_from_base");
        cfg.clear_boot_order();
        cfg.native_acpi_tables(None);

        let mut source = ctx.spawn_vm(&cfg, Some(&env)).await?;
        source.launch().await?;
        source.wait_to_boot().await?;

        ctx.lifecycle_test(
            source,
            &[
                Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT),
                Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT),
            ],
            |vm| {
                Box::pin(async {
                    let InstanceSpecStatus::Present(spec) =
                        vm.get_spec().await.unwrap().spec
                    else {
                        panic!("should have a spec");
                    };
                    assert!(spec.board.native_acpi_tables != Some(true));
                })
            },
        )
        .await?;
    }
}
