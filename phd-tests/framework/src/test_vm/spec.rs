// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::{
    disk::{self, DiskConfig},
    guest_os::GuestOsKind,
};
use propolis_client::types::{
    InstanceMetadata, InstanceSpecV0, StorageBackendV0,
};
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

    /// The contents of the config TOML to write to run this VM.
    pub config_toml_contents: String,

    /// Metadata used to track instance timeseries data.
    pub metadata: InstanceMetadata,
}

impl VmSpec {
    /// Update the Crucible backend specs in the instance spec to match the
    /// current backend specs given by this specification's disk handles.
    pub(crate) fn refresh_crucible_backends(&mut self) {
        for disk in &self.disk_handles {
            let disk = if let Some(disk) = disk.as_crucible() {
                disk
            } else {
                continue;
            };

            let (backend_name, backend_spec) = disk.backend_spec();
            match self
                .instance_spec
                .backends
                .storage_backends
                .get(&backend_name)
            {
                Some(StorageBackendV0::Crucible(_)) => {
                    self.instance_spec
                        .backends
                        .storage_backends
                        .insert(backend_name, backend_spec);
                }
                Some(_) | None => {}
            }
        }
    }

    /// Generate new sled-identifiers for self.
    ///
    /// This creates new metadata for the instance appropriate for a successor
    /// VM. In that case, the sled identifiers will be different, but the
    /// project / instance identifiers should be the same.
    pub(crate) fn refresh_sled_identifiers(&mut self) {
        let id = Uuid::new_v4();
        self.metadata.sled_id = id;
        self.metadata.sled_serial = id.to_string();
    }
}
