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
    types::{ComponentV0, DiskRequest, InstanceMetadata, InstanceSpecV0, Slot},
    PciPath,
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

    /// The bootrom path to pass to this VM's Propolis server processes.
    pub bootrom_path: Utf8PathBuf,

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
            match pci_path.device() {
                dev @ 0x10..=0x17 => Ok(Slot(dev - 0x10)),
                _ => Err(anyhow::anyhow!(
                    "PCI device number {} out of range",
                    pci_path.device()
                )),
            }
        }

        fn get_device_info(device: &ComponentV0) -> anyhow::Result<DeviceInfo> {
            match device {
                ComponentV0::VirtioDisk(d) => Ok(DeviceInfo {
                    backend_name: &d.backend_name,
                    interface: "virtio",
                    slot: convert_to_slot(d.pci_path)?,
                }),
                ComponentV0::NvmeDisk(d) => Ok(DeviceInfo {
                    backend_name: &d.backend_name,
                    interface: "nvme",
                    slot: convert_to_slot(d.pci_path)?,
                }),
                _ => {
                    panic!("asked to get device info for a non-storage device")
                }
            }
        }

        let mut reqs = vec![];
        for (name, device) in
            self.instance_spec.components.iter().filter(|(_, c)| {
                matches!(
                    c,
                    ComponentV0::VirtioDisk(_) | ComponentV0::NvmeDisk(_)
                )
            })
        {
            let info = get_device_info(device)?;
            let backend = self
                .instance_spec
                .components
                .get(info.backend_name)
                .expect("storage device should have a matching backend");

            let ComponentV0::CrucibleStorageBackend(backend) = backend else {
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
