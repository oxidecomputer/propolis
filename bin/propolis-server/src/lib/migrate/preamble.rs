// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeSet;

use propolis_client::instance_spec::{
    migration::{CollectionCompatibilityError, MigrationCompatibilityError},
    v0::{DeviceSpecV0, InstanceSpecV0},
    VersionedInstanceSpec,
};
use serde::{Deserialize, Serialize};
use tokio::sync::MutexGuard;

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct Preamble {
    pub device_spec: DeviceSpecV0,
    pub backend_keys: BTreeSet<String>,
    pub blobs: Vec<Vec<u8>>,
}

fn get_spec_backend_keys(spec: &InstanceSpecV0) -> BTreeSet<String> {
    spec.backends
        .storage_backends
        .keys()
        .chain(spec.backends.network_backends.keys())
        .cloned()
        .collect()
}

impl Preamble {
    pub fn new(instance_spec: VersionedInstanceSpec) -> Preamble {
        let VersionedInstanceSpec::V0(instance_spec) = instance_spec;
        Preamble {
            device_spec: instance_spec.devices.clone(),
            backend_keys: get_spec_backend_keys(&instance_spec),
            blobs: Vec::new(),
        }
    }

    pub fn is_migration_compatible(
        &self,
        other_spec: MutexGuard<'_, VersionedInstanceSpec>,
    ) -> Result<(), MigrationCompatibilityError> {
        let VersionedInstanceSpec::V0(other_spec) = &*other_spec;

        self.device_spec.can_migrate_devices_from(&other_spec.devices)?;
        let other_keys = get_spec_backend_keys(other_spec);
        if self.backend_keys.len() != other_keys.len() {
            return Err(MigrationCompatibilityError::CollectionMismatch(
                "backends".to_string(),
                CollectionCompatibilityError::CollectionSize(
                    self.backend_keys.len(),
                    other_keys.len(),
                ),
            ));
        }

        for this_key in self.backend_keys.iter() {
            if !other_keys.contains(this_key) {
                return Err(MigrationCompatibilityError::CollectionMismatch(
                    "backends".to_string(),
                    CollectionCompatibilityError::CollectionKeyAbsent(
                        this_key.clone(),
                    ),
                ));
            }
        }

        // TODO: Compare opaque blobs.

        Ok(())
    }
}
