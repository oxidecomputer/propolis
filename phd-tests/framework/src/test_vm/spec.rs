// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::{
    disk::{self, DiskConfig},
    guest_os::GuestOsKind,
};
use camino::Utf8PathBuf;
use propolis_client::{
    instance_spec::{ComponentV0, InstanceSpecV0},
    types::InstanceMetadata,
};
use uuid::Uuid;

/// The set of objects needed to start and run a guest in a `TestVm`.
#[derive(Clone)]
pub struct VmSpec {
    pub vm_name: String,

    /// The instance spec to pass to the VM when starting the guest.
    base_instance_spec: InstanceSpecV0,

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
    pub fn get_disk_by_device_name(
        &self,
        name: &str,
    ) -> Option<&Arc<dyn disk::DiskConfig>> {
        self.disk_handles
            .iter()
            .find(|disk| disk.device_name().as_str() == name)
    }

    pub(crate) fn new(
        vm_name: String,
        instance_spec: InstanceSpecV0,
        disk_handles: Vec<Arc<dyn disk::DiskConfig>>,
        guest_os_kind: GuestOsKind,
        bootrom_path: Utf8PathBuf,
        metadata: InstanceMetadata,
    ) -> Self {
        Self {
            vm_name,
            base_instance_spec: instance_spec,
            disk_handles,
            guest_os_kind,
            bootrom_path,
            metadata,
        }
    }

    pub(crate) fn set_vm_name(&mut self, name: String) {
        self.vm_name = name
    }

    pub(crate) fn instance_spec(&self) -> InstanceSpecV0 {
        let mut spec = self.base_instance_spec.clone();
        self.set_crucible_backends(&mut spec);
        spec
    }

    /// Update the Crucible backend specs in the instance spec to match the
    /// current backend specs given by this specification's disk handles.
    fn set_crucible_backends(&self, spec: &mut InstanceSpecV0) {
        for disk in &self.disk_handles {
            let disk = if let Some(disk) = disk.as_crucible() {
                disk
            } else {
                continue;
            };

            let backend_spec = disk.backend_spec();
            let backend_name = disk
                .device_name()
                .clone()
                .into_backend_name()
                .into_string()
                .into();
            if let Some(ComponentV0::CrucibleStorageBackend(_)) =
                spec.components.get(&backend_name)
            {
                spec.components.insert(backend_name, backend_spec);
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
