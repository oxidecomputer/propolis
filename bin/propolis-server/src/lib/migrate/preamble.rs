// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use propolis_client::instance_spec::{
    BackendNames, DeviceSpec, InstanceSpec, MigrationCompatibilityError,
};
use serde::{Deserialize, Serialize};
use tokio::sync::MutexGuard;

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct Preamble {
    pub device_spec: DeviceSpec,
    pub backend_keys: BackendNames,
    pub blobs: Vec<Vec<u8>>,
}

impl Preamble {
    pub fn new(instance_spec: InstanceSpec) -> Preamble {
        Preamble {
            device_spec: instance_spec.devices.clone(),
            backend_keys: BackendNames::from(&instance_spec.backends),
            blobs: Vec::new(),
        }
    }

    pub fn is_migration_compatible(
        &self,
        other_spec: MutexGuard<'_, InstanceSpec>,
    ) -> Result<(), MigrationCompatibilityError> {
        self.device_spec.can_migrate_devices_from(&other_spec.devices)?;
        let other_keys = BackendNames::from(&other_spec.backends);
        self.backend_keys.can_migrate_backends_from(&other_keys)?;

        // TODO: Compare opaque blobs.

        Ok(())
    }
}
