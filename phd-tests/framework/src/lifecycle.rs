// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

use tracing::info;
use uuid::Uuid;

use crate::{Framework, TestVm};

/// The set of actions that can be taken on a VM undergoing lifecycle testing.
pub enum Action<'a> {
    /// Reset the VM using the Propolis server reset API. This sort of reboot
    /// does not involve the guest OS.
    Reset,

    /// Stop the VM and restart it in a successor Propolis using the same
    /// environment as its predecessor.
    StopAndStart,

    /// Migrate the VM to a new Propolis server. The wrapped `&str` names a
    /// Propolis server artifact to migrate to.
    MigrateToPropolis(&'a str),
}

impl Framework {
    /// Runs a lifecycle test on the supplied `vm` by iterating over the
    /// `actions`, performing the specified action, and then calling `check_fn`
    /// on the resulting VM to verify invariants.
    pub fn lifecycle_test<F>(
        &self,
        vm: TestVm,
        actions: &[Action],
        check_fn: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(&TestVm) -> (),
    {
        let mut vm = vm;
        let original_name = vm.name().to_owned();
        for (idx, action) in actions.into_iter().enumerate() {
            match action {
                Action::Reset => {
                    info!(
                        vm_name = original_name,
                        "rebooting VM for lifecycle test"
                    );
                    vm.reset()?;
                }
                Action::StopAndStart => {
                    info!(
                        vm_name = original_name,
                        "stopping and starting VM for lifecycle test"
                    );
                    let new_vm_name =
                        format!("{}_lifecycle_{}", original_name, idx);
                    vm.stop()?;
                    let mut new_vm =
                        self.spawn_successor_vm(&new_vm_name, &vm, None)?;
                    new_vm.launch()?;
                    new_vm.wait_to_boot()?;
                    vm = new_vm;
                }
                Action::MigrateToPropolis(propolis) => {
                    info!(
                        vm_name = original_name,
                        propolis_artifact = propolis,
                        "migrating to new Propolis artifact for lifecycle test"
                    );

                    let new_vm_name =
                        format!("{}_lifecycle_{}", original_name, idx);

                    let mut env = self.environment_builder();
                    env.propolis(propolis);
                    let mut new_vm =
                        self.spawn_successor_vm(&new_vm_name, &vm, Some(&env))?;

                    new_vm.migrate_from(
                        &vm,
                        Uuid::new_v4(),
                        Duration::from_secs(120),
                    )?;
                    vm = new_vm;
                }
            }

            check_fn(&vm);
        }

        Ok(())
    }
}
