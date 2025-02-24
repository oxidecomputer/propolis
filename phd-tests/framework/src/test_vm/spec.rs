// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::{
    disk::{self, DiskConfig},
    guest_os::GuestOsKind,
};
use camino::Utf8PathBuf;
use propolis_client::types::{ComponentV0, InstanceMetadata, InstanceSpecV0};
use uuid::Uuid;

/// The set of objects needed to start and run a guest in a `TestVm`.
#[derive(Clone)]
pub struct VmSpec {
    pub vm_name: String,

    /// The instance spec to pass to the VM when starting the guest.
    pub instance_spec: InstanceSpecV0,

    /// A set of handles to disk files that the VM's disk backends refer to.
    pub disk_handles: Vec<Arc<dyn disk::DiskConfig>>,

    /// The guest OS adapter to use for the VM.
    pub guest_os_kind: GuestOsKind,

    /// The bootrom path to pass to this VM's Propolis server processes.
    pub bootrom_path: Utf8PathBuf,

    /// Metadata used to track instance timeseries data.
    pub metadata: InstanceMetadata,
}

impl VmSpec {
    /// Yields an array of handles to this VM's Crucible disks.
    pub fn get_crucible_disks(&self) -> Vec<Arc<dyn disk::DiskConfig>> {
        self.disk_handles
            .iter()
            .filter(|d| d.as_crucible().is_some())
            .cloned()
            .collect()
    }

    /// Update the Crucible backend specs in the instance spec to match the
    /// current backend specs given by this specification's disk handles.
    pub(crate) fn refresh_crucible_backends(&mut self) {
        for disk in &self.disk_handles {
            let disk = if let Some(disk) = disk.as_crucible() {
                disk
            } else {
                continue;
            };

            let backend_spec = disk.backend_spec();
            let backend_name =
                disk.device_name().clone().into_backend_name().into_string();
            if let Some(ComponentV0::CrucibleStorageBackend(_)) =
                self.instance_spec.components.get(&backend_name)
            {
                self.instance_spec
                    .components
                    .insert(backend_name, backend_spec);
            }
        }
    }

    /// Generate new sled-identifiers for self.
    ///
    /// This creates new metadata for the instance appropriate for a successor
    /// VM. In that case, the sled identifiers will be different, but the
    /// project / instance identifiers should be the same.
    ///
    /// This models the case we're interested in during a migration: the
    /// sled-agent will provide the same silo / project / instance IDs to that
    /// destination Propolis, but it will have different sled identifiers,
    /// because the hosting sled differs. In general, all the IDs could be
    /// different, but we only change the actual ID and serial number here.
    pub(crate) fn refresh_sled_identifiers(&mut self) {
        let id = Uuid::new_v4();
        self.metadata.sled_id = id;
        self.metadata.sled_serial = id.to_string();
    }
}
