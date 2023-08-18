// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from native instance spec types to Progenitor-generated API
//! types.

use super::{v0::*, *};
use crate::types as api;

// Convenience macro for converting from a HashMap<K, A> to a HashMap<K, B>
// where A implements Into<B>. (In this module, A is a native instance spec type
// and B is its corresponding Progenitor-generated type.)
macro_rules! convert_map {
    ($i:ident) => {
        ($i).into_iter().map(|(k, v)| (k, v.into())).collect()
    };
}

impl From<VersionedInstanceSpec> for api::VersionedInstanceSpec {
    fn from(spec: VersionedInstanceSpec) -> Self {
        match spec {
            VersionedInstanceSpec::V0(spec) => {
                api::VersionedInstanceSpec::V0(spec.into())
            }
        }
    }
}

impl From<InstanceSpecV0> for api::InstanceSpecV0 {
    fn from(spec: InstanceSpecV0) -> Self {
        let InstanceSpecV0 { devices, backends } = spec;
        api::InstanceSpecV0 {
            backends: backends.into(),
            devices: devices.into(),
        }
    }
}

impl From<BackendSpecV0> for api::BackendSpecV0 {
    fn from(spec: BackendSpecV0) -> Self {
        let BackendSpecV0 { storage_backends, network_backends } = spec;
        api::BackendSpecV0 {
            storage_backends: convert_map!(storage_backends),
            network_backends: convert_map!(network_backends),
        }
    }
}

impl From<DeviceSpecV0> for api::DeviceSpecV0 {
    fn from(spec: DeviceSpecV0) -> Self {
        let DeviceSpecV0 {
            board,
            storage_devices,
            network_devices,
            serial_ports,
            pci_pci_bridges,
            // TODO(#264) If this library is built with the `falcon` feature
            // enabled, `DeviceSpec` will contain fields thare are not in the
            // generated `api::DeviceSpec` type. Just drop these for now until
            // there is a solid plan for dealing with conditionally-compiled
            // instance spec elements.
            ..
        } = spec;

        api::DeviceSpecV0 {
            board: board.into(),
            storage_devices: convert_map!(storage_devices),
            network_devices: convert_map!(network_devices),
            pci_pci_bridges: convert_map!(pci_pci_bridges),
            serial_ports: convert_map!(serial_ports),
        }
    }
}

impl From<StorageBackendV0> for api::StorageBackendV0 {
    fn from(be: StorageBackendV0) -> Self {
        match be {
            StorageBackendV0::Crucible(spec) => {
                api::StorageBackendV0::Crucible(spec.into())
            }
            StorageBackendV0::File(spec) => {
                api::StorageBackendV0::File(spec.into())
            }
            StorageBackendV0::InMemory(spec) => {
                api::StorageBackendV0::InMemory(spec.into())
            }
        }
    }
}

impl From<components::backends::CrucibleStorageBackend>
    for api::CrucibleStorageBackend
{
    fn from(be: components::backends::CrucibleStorageBackend) -> Self {
        api::CrucibleStorageBackend {
            request_json: be.request_json,
            readonly: be.readonly,
        }
    }
}

impl From<components::backends::FileStorageBackend>
    for api::FileStorageBackend
{
    fn from(be: components::backends::FileStorageBackend) -> Self {
        api::FileStorageBackend { path: be.path, readonly: be.readonly }
    }
}

impl From<components::backends::BlobStorageBackend>
    for api::InMemoryStorageBackend
{
    fn from(be: components::backends::BlobStorageBackend) -> Self {
        api::InMemoryStorageBackend { base64: be.base64, readonly: be.readonly }
    }
}

impl From<NetworkBackendV0> for api::NetworkBackendV0 {
    fn from(be: NetworkBackendV0) -> Self {
        match be {
            NetworkBackendV0::Virtio(spec) => {
                api::NetworkBackendV0::Virtio(spec.into())
            }
            NetworkBackendV0::Dlpi(spec) => {
                api::NetworkBackendV0::Dlpi(spec.into())
            }
        }
    }
}

impl From<components::backends::VirtioNetworkBackend>
    for api::VirtioNetworkBackend
{
    fn from(be: components::backends::VirtioNetworkBackend) -> Self {
        api::VirtioNetworkBackend { vnic_name: be.vnic_name }
    }
}

impl From<components::backends::DlpiNetworkBackend>
    for api::DlpiNetworkBackend
{
    fn from(be: components::backends::DlpiNetworkBackend) -> Self {
        api::DlpiNetworkBackend { vnic_name: be.vnic_name }
    }
}

impl From<components::board::Board> for api::Board {
    fn from(board: components::board::Board) -> Self {
        let components::board::Board { cpus, memory_mb, chipset } = board;
        api::Board { cpus, memory_mb, chipset: chipset.into() }
    }
}

impl From<components::board::Chipset> for api::Chipset {
    fn from(chipset: components::board::Chipset) -> Self {
        match chipset {
            components::board::Chipset::I440Fx(i440fx) => {
                api::Chipset::I440Fx(api::I440Fx {
                    enable_pcie: i440fx.enable_pcie,
                })
            }
        }
    }
}

impl From<StorageDeviceV0> for api::StorageDeviceV0 {
    fn from(device: StorageDeviceV0) -> Self {
        match device {
            StorageDeviceV0::VirtioDisk(disk) => {
                api::StorageDeviceV0::VirtioDisk(disk.into())
            }
            StorageDeviceV0::NvmeDisk(disk) => {
                api::StorageDeviceV0::NvmeDisk(disk.into())
            }
        }
    }
}

impl From<components::devices::VirtioDisk> for api::VirtioDisk {
    fn from(disk: components::devices::VirtioDisk) -> Self {
        api::VirtioDisk {
            backend_name: disk.backend_name,
            pci_path: disk.pci_path.into(),
        }
    }
}

impl From<components::devices::NvmeDisk> for api::NvmeDisk {
    fn from(disk: components::devices::NvmeDisk) -> Self {
        api::NvmeDisk {
            backend_name: disk.backend_name,
            pci_path: disk.pci_path.into(),
        }
    }
}

impl From<NetworkDeviceV0> for api::NetworkDeviceV0 {
    fn from(device: NetworkDeviceV0) -> Self {
        match device {
            NetworkDeviceV0::VirtioNic(nic) => {
                api::NetworkDeviceV0::VirtioNic(nic.into())
            }
        }
    }
}

impl From<components::devices::VirtioNic> for api::VirtioNic {
    fn from(nic: components::devices::VirtioNic) -> Self {
        api::VirtioNic {
            backend_name: nic.backend_name,
            pci_path: nic.pci_path.into(),
        }
    }
}

impl From<components::devices::PciPciBridge> for api::PciPciBridge {
    fn from(bridge: components::devices::PciPciBridge) -> Self {
        api::PciPciBridge {
            downstream_bus: bridge.downstream_bus,
            pci_path: bridge.pci_path.into(),
        }
    }
}

impl From<components::devices::SerialPort> for api::SerialPort {
    fn from(port: components::devices::SerialPort) -> Self {
        api::SerialPort { num: port.num.into() }
    }
}

impl From<components::devices::SerialPortNumber> for api::SerialPortNumber {
    fn from(num: components::devices::SerialPortNumber) -> Self {
        match num {
            components::devices::SerialPortNumber::Com1 => {
                api::SerialPortNumber::Com1
            }
            components::devices::SerialPortNumber::Com2 => {
                api::SerialPortNumber::Com2
            }
            components::devices::SerialPortNumber::Com3 => {
                api::SerialPortNumber::Com3
            }
            components::devices::SerialPortNumber::Com4 => {
                api::SerialPortNumber::Com4
            }
        }
    }
}

impl From<PciPath> for api::PciPath {
    fn from(path: PciPath) -> Self {
        api::PciPath {
            bus: path.bus(),
            device: path.device(),
            function: path.function(),
        }
    }
}

// Crucible type conversions.

use crate::types::VolumeConstructionRequest as GenVCR;
use crucible_client_types::VolumeConstructionRequest as CrucibleVCR;

impl From<CrucibleVCR> for GenVCR {
    fn from(vcr: CrucibleVCR) -> Self {
        match vcr {
            CrucibleVCR::Volume {
                id,
                block_size,
                sub_volumes,
                read_only_parent,
            } => GenVCR::Volume {
                id,
                block_size,
                sub_volumes: sub_volumes.into_iter().map(Into::into).collect(),
                read_only_parent: read_only_parent
                    .map(|rop| Box::new((*rop).into())),
            },
            CrucibleVCR::Url { id, block_size, url } => {
                GenVCR::Url { id, block_size, url }
            }
            CrucibleVCR::Region {
                block_size,
                blocks_per_extent,
                extent_count,
                opts,
                gen,
            } => GenVCR::Region {
                block_size,
                blocks_per_extent,
                extent_count,
                opts: opts.into(),
                gen,
            },
            CrucibleVCR::File { id, block_size, path } => {
                GenVCR::File { id, block_size, path }
            }
        }
    }
}

impl From<crucible_client_types::CrucibleOpts> for crate::types::CrucibleOpts {
    fn from(opts: crucible_client_types::CrucibleOpts) -> Self {
        let crucible_client_types::CrucibleOpts {
            id,
            target,
            lossy,
            flush_timeout,
            key,
            cert_pem,
            key_pem,
            root_cert_pem,
            control,
            read_only,
        } = opts;
        crate::types::CrucibleOpts {
            id,
            target: target.into_iter().map(|t| t.to_string()).collect(),
            lossy,
            flush_timeout: flush_timeout.map(|x| x.into()),
            key,
            cert_pem,
            key_pem,
            root_cert_pem,
            control: control.map(|c| c.to_string()),
            read_only,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{v0::builder::SpecBuilder, *};

    #[test]
    fn device_specs_are_convertible() {
        let mut builder = SpecBuilder::new(4, 4096, false);
        builder.add_storage_device(
            "disk1".to_string(),
            StorageDeviceV0::NvmeDisk(components::devices::NvmeDisk {
                backend_name: "disk1_be".to_string(),
                pci_path: PciPath::new(0, 16, 0).unwrap(),
            }),
            "disk1_be".to_string(),
            StorageBackendV0::Crucible(
                components::backends::CrucibleStorageBackend {
                    request_json: serde_json::to_string(
                        &crucible_client_types::VolumeConstructionRequest::Region {
                            block_size: 512,
                            blocks_per_extent: 20,
                            extent_count: 40,
                            opts: crucible_client_types::CrucibleOpts {
                                id: uuid::Uuid::new_v4(),
                                target: vec![],
                                lossy: false,
                                flush_timeout: None,
                                key: None,
                                cert_pem: None,
                                key_pem: None,
                                root_cert_pem: None,
                                control: None,
                                read_only: true,
                            },
                            gen: 1,
                        },
                    )
                    .unwrap(),
                    readonly: true,
                },
            ),
        ).unwrap();

        let spec = builder.finish();
        let api_spec: api::InstanceSpecV0 = spec.into();
        assert_eq!(api_spec.devices.storage_devices.len(), 1);
    }
}
