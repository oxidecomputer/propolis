// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use propolis_api_types::instance_spec::{
    v0::ComponentV0, VersionedInstanceSpec,
};
use serde::{Deserialize, Serialize};

use crate::spec::{api_spec_v0::ApiSpecError, Spec, StorageBackend};

use super::MigrateError;

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct Preamble {
    pub instance_spec: VersionedInstanceSpec,
    pub blobs: Vec<Vec<u8>>,
}

impl Preamble {
    pub fn new(instance_spec: VersionedInstanceSpec) -> Preamble {
        Preamble { instance_spec, blobs: Vec::new() }
    }

    /// Given the instance spec in this preamble (transmitted from the migration
    /// source), replace any Crucible and viona backends with their
    ///
    /// This check runs on the destination.
    pub fn get_amended_spec(
        self,
        target_spec: &Spec,
    ) -> Result<Spec, MigrateError> {
        let VersionedInstanceSpec::V0(mut source_spec) = self.instance_spec;
        for disk in target_spec.disks.values() {
            let StorageBackend::Crucible(crucible) = &disk.backend_spec else {
                continue;
            };

            let Some(to_amend) =
                source_spec.components.get_mut(disk.device_spec.backend_name())
            else {
                return Err(MigrateError::InstanceSpecsIncompatible(format!(
                    "replacement component {} not in source spec",
                    disk.device_spec.backend_name()
                )));
            };

            if !matches!(to_amend, ComponentV0::CrucibleStorageBackend(_)) {
                return Err(MigrateError::InstanceSpecsIncompatible(format!(
                    "component {} is not a Crucible backend in the source spec",
                    disk.device_spec.backend_name()
                )));
            }

            *to_amend = ComponentV0::CrucibleStorageBackend(crucible.clone());
        }

        for nic in target_spec.nics.values() {
            let Some(to_amend) =
                source_spec.components.get_mut(&nic.device_spec.backend_name)
            else {
                return Err(MigrateError::InstanceSpecsIncompatible(format!(
                    "replacement component {} not in source spec",
                    nic.device_spec.backend_name
                )));
            };

            if !matches!(to_amend, ComponentV0::VirtioNetworkBackend(_)) {
                return Err(MigrateError::InstanceSpecsIncompatible(format!(
                    "component {} is not a virtio network backend \
                            in the source spec",
                    nic.device_spec.backend_name
                )));
            }

            *to_amend =
                ComponentV0::VirtioNetworkBackend(nic.backend_spec.clone());
        }

        let amended_spec =
            source_spec.try_into().map_err(|e: ApiSpecError| {
                MigrateError::PreambleParse(e.to_string())
            })?;

        // TODO: Compare opaque blobs.

        Ok(amended_spec)
    }
}
