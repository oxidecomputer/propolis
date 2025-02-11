// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Context;
use futures::future::BoxFuture;
use tracing::info;
use uuid::Uuid;

use crate::{test_vm::MigrationTimeout, Framework, TestVm};

/// The set of actions that can be taken on a VM undergoing lifecycle testing.
pub enum Action<'a> {
    /// Reset the VM using the Propolis server reset API. This sort of reboot
    /// does not involve the guest OS. It can be used to verify that components'
    /// reset implementations don't change properties that shouldn't change
    /// without fully stopping and restarting a VM.
    Reset,

    /// Stop the VM and restart it in a successor Propolis using the same
    /// environment as its predecessor.
    StopAndStart,

    /// Migrate the VM to a new Propolis server. The wrapped `&str` names a
    /// Propolis server artifact to migrate to.
    //
    // N.B. This isn't used in any lifecycle tests yet, mostly because there are
    // no well-known, stable Propolis artifact names other than the name of the
    // default artifact supplied on the command line. This will change in the
    // future as new well-known artifacts (like "Buildomat HEAD") are added.
    MigrateToPropolis(&'a str),
}

impl Framework {
    /// Runs a lifecycle test on the supplied `vm` by iterating over the
    /// `actions`, performing the specified action, and then calling `check_fn`
    /// on the resulting VM to verify invariants.
    pub async fn lifecycle_test<'a>(
        &self,
        vm: TestVm,
        actions: &[Action<'_>],
        check_fn: impl for<'v> Fn(&'v TestVm) -> BoxFuture<'v, ()>,
    ) -> anyhow::Result<()> {
        let mut vm = vm;
        let original_name = vm.name().to_owned();
        for (idx, action) in actions.iter().enumerate() {
            match action {
                Action::Reset => {
                    info!(
                        vm_name = original_name,
                        "rebooting VM for lifecycle test"
                    );
                    vm.reset().await?;
                    vm.wait_to_boot().await?;
                }
                Action::StopAndStart => {
                    info!(
                        vm_name = original_name,
                        "stopping and starting VM for lifecycle test"
                    );
                    let new_vm_name =
                        format!("{}_lifecycle_{}", original_name, idx);
                    vm.stop().await?;
                    let mut new_vm = self
                        .spawn_successor_vm(&new_vm_name, &vm, None)
                        .await?;
                    new_vm.launch().await?;
                    new_vm.wait_to_boot().await?;
                    vm = new_vm;
                }
                Action::MigrateToPropolis(propolis) => {
                    use propolis_client::types::MigrationState;
                    info!(
                        vm_name = original_name,
                        propolis_artifact = propolis,
                        "migrating to new Propolis artifact for lifecycle test"
                    );

                    let new_vm_name =
                        format!("{}_lifecycle_{}", original_name, idx);

                    let mut env = self.environment_builder();
                    env.propolis(propolis);
                    let mut new_vm = self
                        .spawn_successor_vm(&new_vm_name, &vm, Some(&env))
                        .await?;
                    let migration_id = Uuid::new_v4();
                    new_vm
                        .migrate_from(
                            &vm,
                            migration_id,
                            MigrationTimeout::default(),
                        )
                        .await?;

                    // Explicitly check migration status on both the source and
                    // target to make sure it is available even after migration
                    // has finished.
                    let src_migration_state = vm
                        .get_migration_state()
                        .await
                        .context("Failed to get source VM migration state")?
                        .migration_out
                        .expect("source VM should have migrated out")
                        .state;
                    assert_eq!(src_migration_state, MigrationState::Finish);

                    let target_migration_state = new_vm
                        .get_migration_state()
                        .await
                        .context("Failed to get target VM migration state")?
                        .migration_in
                        .expect("target VM should have migrated in")
                        .state;
                    assert_eq!(target_migration_state, MigrationState::Finish);

                    vm = new_vm;
                }
            }

            check_fn(&vm).await;
        }

        Ok(())
    }
}
