// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::{
    disk::{self, DiskConfig},
    guest_os::GuestOsKind,
};
use propolis_client::types::{
    DiskRequest, InstanceMetadata, InstanceSpecV0, PciPath, Slot,
    StorageBackendV0, StorageDeviceV0,
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

    /// Generates a set of [`propolis_client::types::DiskRequest`] structures
    /// corresponding to the disks in this VM spec.
    ///
    /// All of the disks in the spec must be Crucible disks. If one is not, this
    /// routine returns an error.
    pub(crate) fn make_disk_requests(
        &self,
    ) -> anyhow::Result<Vec<DiskRequest>> {
        struct DeviceInfo<'a> {
            backend_name: &'a str,
            interface: &'static str,
            slot: Slot,
        }

        fn convert_to_slot(pci_path: PciPath) -> anyhow::Result<Slot> {
            match pci_path.device {
                dev @ 0x10..=0x17 => Ok(Slot(dev - 0x10)),
                _ => Err(anyhow::anyhow!(
                    "PCI device number {} out of range",
                    pci_path.device
                )),
            }
        }

        fn get_device_info(
            device: &StorageDeviceV0,
        ) -> anyhow::Result<DeviceInfo> {
            match device {
                StorageDeviceV0::VirtioDisk(d) => Ok(DeviceInfo {
                    backend_name: &d.backend_name,
                    interface: "virtio",
                    slot: convert_to_slot(d.pci_path)?,
                }),
                StorageDeviceV0::NvmeDisk(d) => Ok(DeviceInfo {
                    backend_name: &d.backend_name,
                    interface: "nvme",
                    slot: convert_to_slot(d.pci_path)?,
                }),
            }
        }

        let mut reqs = vec![];
        for (name, device) in self.instance_spec.devices.storage_devices.iter()
        {
            let info = get_device_info(device)?;
            let backend = self
                .instance_spec
                .backends
                .storage_backends
                .get(info.backend_name)
                .expect("storage device should have a matching backend");

            let StorageBackendV0::Crucible(backend) = backend else {
                anyhow::bail!("disk {name} does not have a Crucible backend");
            };

            reqs.push(DiskRequest {
                device: info.interface.to_owned(),
                name: name.clone(),
                read_only: backend.readonly,
                slot: info.slot,
                volume_construction_request: serde_json::from_str(
                    &backend.request_json,
                )?,
            })
        }

        Ok(reqs)
    }
}
