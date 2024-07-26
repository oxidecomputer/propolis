// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Converts device descriptions from an
//! [`propolis_api_types::InstanceEnsureRequest`] into elements that can be
//! added to a spec.

use propolis_api_types::{
    instance_spec::{
        components::{
            backends::{
                BlobStorageBackend, CrucibleStorageBackend,
                VirtioNetworkBackend,
            },
            devices::{NvmeDisk, VirtioDisk, VirtioNic},
        },
        v0::{NetworkDeviceV0, StorageBackendV0, StorageDeviceV0},
        PciPath,
    },
    DiskRequest, NetworkInterfaceRequest, Slot,
};
use thiserror::Error;

use super::{Disk, Nic};

#[derive(Debug, Error)]
pub(crate) enum DeviceRequestError {
    #[error("invalid storage interface {0} for disk in slot {1}")]
    InvalidStorageInterface(String, u8),

    #[error("invalid PCI slot {0} for device type {1:?}")]
    PciSlotInvalid(u8, SlotType),

    #[error("error serializing {0}")]
    SerializationError(String, #[source] serde_json::error::Error),
}

/// A type of PCI device. Device numbers on the PCI bus are partitioned by slot
/// type. If a client asks to attach a device of type X to PCI slot Y, the
/// server will assign the Yth device number in X's partition. The partitioning
/// scheme is defined by the implementation of the `slot_to_pci_path` utility
/// function.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SlotType {
    Nic,
    Disk,
    CloudInit,
}

/// Translates a device type and PCI slot (as presented in an instance creation
/// request) into a concrete PCI path. See the documentation for [`SlotType`].
fn slot_to_pci_path(
    slot: Slot,
    ty: SlotType,
) -> Result<PciPath, DeviceRequestError> {
    match ty {
        // Slots for NICS: 0x08 -> 0x0F
        SlotType::Nic if slot.0 <= 7 => PciPath::new(0, slot.0 + 0x8, 0),
        // Slots for Disks: 0x10 -> 0x17
        SlotType::Disk if slot.0 <= 7 => PciPath::new(0, slot.0 + 0x10, 0),
        // Slot for CloudInit
        SlotType::CloudInit if slot.0 == 0 => PciPath::new(0, slot.0 + 0x18, 0),
        _ => return Err(DeviceRequestError::PciSlotInvalid(slot.0, ty)),
    }
    .map_err(|_| DeviceRequestError::PciSlotInvalid(slot.0, ty))
}

pub(super) fn parse_disk_from_request(
    disk: &DiskRequest,
) -> Result<Disk, DeviceRequestError> {
    let pci_path = slot_to_pci_path(disk.slot, SlotType::Disk)?;
    let backend_name = format!("{}-backend", disk.name);
    let device_spec = match disk.device.as_ref() {
        "virtio" => StorageDeviceV0::VirtioDisk(VirtioDisk {
            backend_name: backend_name.clone(),
            pci_path,
        }),
        "nvme" => StorageDeviceV0::NvmeDisk(NvmeDisk {
            backend_name: backend_name.clone(),
            pci_path,
        }),
        _ => {
            return Err(DeviceRequestError::InvalidStorageInterface(
                disk.device.clone(),
                disk.slot.0,
            ))
        }
    };

    let device_name = disk.name.clone();
    let backend_spec = StorageBackendV0::Crucible(CrucibleStorageBackend {
        request_json: serde_json::to_string(&disk.volume_construction_request)
            .map_err(|e| {
                DeviceRequestError::SerializationError(disk.name.clone(), e)
            })?,
        readonly: disk.read_only,
    });

    Ok(Disk { device_name, device_spec, backend_name, backend_spec })
}

pub(super) fn parse_cloud_init_from_request(
    base64: String,
) -> Result<Disk, DeviceRequestError> {
    let name = "cloud-init";
    let pci_path = slot_to_pci_path(Slot(0), SlotType::CloudInit)?;
    let backend_name = name.to_string();
    let backend_spec =
        StorageBackendV0::Blob(BlobStorageBackend { base64, readonly: true });

    let device_name = name.to_string();
    let device_spec = StorageDeviceV0::VirtioDisk(VirtioDisk {
        backend_name: name.to_string(),
        pci_path,
    });

    Ok(Disk { device_name, device_spec, backend_name, backend_spec })
}

pub(super) fn parse_nic_from_request(
    nic: &NetworkInterfaceRequest,
) -> Result<Nic, DeviceRequestError> {
    let pci_path = slot_to_pci_path(nic.slot, SlotType::Nic)?;
    let (device_name, backend_name) = super::pci_path_to_nic_names(pci_path);
    let device_spec = NetworkDeviceV0::VirtioNic(VirtioNic {
        backend_name: backend_name.clone(),
        interface_id: nic.interface_id,
        pci_path,
    });

    let backend_spec = VirtioNetworkBackend { vnic_name: nic.name.to_string() };
    Ok(Nic { device_name, device_spec, backend_name, backend_spec })
}

#[cfg(test)]
mod test {
    use propolis_api_types::VolumeConstructionRequest;
    use uuid::Uuid;

    use super::*;

    fn check_parsed_storage_device_backend_pointer(parsed: &Disk) {
        let device_to_backend = match &parsed.device_spec {
            StorageDeviceV0::VirtioDisk(d) => d.backend_name.clone(),
            StorageDeviceV0::NvmeDisk(d) => d.backend_name.clone(),
        };

        assert_eq!(device_to_backend, parsed.backend_name);
    }

    #[test]
    fn parsed_disk_devices_point_to_backends() {
        let vcr = VolumeConstructionRequest::File {
            id: Uuid::nil(),
            block_size: 512,
            path: "".to_string(),
        };

        let req = DiskRequest {
            name: "my-disk".to_string(),
            slot: Slot(0),
            read_only: false,
            device: "nvme".to_string(),
            volume_construction_request: vcr,
        };

        let parsed = parse_disk_from_request(&req).unwrap();
        check_parsed_storage_device_backend_pointer(&parsed);
    }

    #[test]
    fn parsed_network_devices_point_to_backends() {
        let req = NetworkInterfaceRequest {
            name: "vnic".to_string(),
            interface_id: uuid::Uuid::new_v4(),
            slot: Slot(0),
        };

        let parsed = parse_nic_from_request(&req).unwrap();
        let NetworkDeviceV0::VirtioNic(nic) = &parsed.device_spec;
        assert_eq!(nic.backend_name, parsed.backend_name);
    }

    #[test]
    fn parsed_cloud_init_devices_point_to_backends() {
        let base64 = "AAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string();
        let parsed = parse_cloud_init_from_request(base64).unwrap();
        check_parsed_storage_device_backend_pointer(&parsed);
    }
}
