//! Conversions from native instance spec types to Progenitor-generated API
//! types.

use super::*;
use crate::types as api;

// Convenience macro for converting from a HashMap<K, A> to a HashMap<K, B>
// where A implements Into<B>. (In this module, A is a native instance spec type
// and B is its corresponding Progenitor-generated type.)
macro_rules! convert_map {
    ($i:ident) => {
        ($i).into_iter().map(|(k, v)| (k, v.into())).collect()
    };
}

impl From<InstanceSpec> for api::InstanceSpec {
    fn from(spec: super::InstanceSpec) -> Self {
        let InstanceSpec { devices, backends } = spec;
        api::InstanceSpec { backends: backends.into(), devices: devices.into() }
    }
}

impl From<BackendSpec> for api::BackendSpec {
    fn from(spec: BackendSpec) -> Self {
        let BackendSpec { storage_backends, network_backends } = spec;
        api::BackendSpec {
            storage_backends: convert_map!(storage_backends),
            network_backends: convert_map!(network_backends),
        }
    }
}

impl From<DeviceSpec> for api::DeviceSpec {
    fn from(spec: DeviceSpec) -> Self {
        let DeviceSpec {
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

        api::DeviceSpec {
            board: board.into(),
            storage_devices: convert_map!(storage_devices),
            network_devices: convert_map!(network_devices),
            pci_pci_bridges: convert_map!(pci_pci_bridges),
            serial_ports: convert_map!(serial_ports),
        }
    }
}

impl From<StorageBackend> for api::StorageBackend {
    fn from(be: StorageBackend) -> Self {
        let StorageBackend { kind, readonly } = be;
        api::StorageBackend { kind: kind.into(), readonly }
    }
}

impl From<StorageBackendKind> for api::StorageBackendKind {
    fn from(kind: StorageBackendKind) -> Self {
        match kind {
            StorageBackendKind::Crucible { req } => {
                api::StorageBackendKind::Crucible { req: req.into() }
            }
            StorageBackendKind::File { path } => {
                api::StorageBackendKind::File { path }
            }
            StorageBackendKind::InMemory { base64 } => {
                api::StorageBackendKind::InMemory { base64 }
            }
        }
    }
}

impl From<NetworkBackend> for api::NetworkBackend {
    fn from(be: NetworkBackend) -> Self {
        api::NetworkBackend { kind: be.kind.into() }
    }
}

impl From<NetworkBackendKind> for api::NetworkBackendKind {
    fn from(kind: NetworkBackendKind) -> Self {
        match kind {
            NetworkBackendKind::Virtio { vnic_name } => {
                api::NetworkBackendKind::Virtio { vnic_name }
            }
            NetworkBackendKind::Dlpi { vnic_name } => {
                api::NetworkBackendKind::Dlpi { vnic_name }
            }
        }
    }
}

impl From<Board> for api::Board {
    fn from(board: Board) -> Self {
        let Board { cpus, memory_mb, chipset } = board;
        api::Board { cpus, memory_mb, chipset: chipset.into() }
    }
}

impl From<Chipset> for api::Chipset {
    fn from(chipset: Chipset) -> Self {
        match chipset {
            Chipset::I440Fx { enable_pcie } => {
                api::Chipset::I440Fx { enable_pcie }
            }
        }
    }
}

impl From<StorageDevice> for api::StorageDevice {
    fn from(device: StorageDevice) -> Self {
        let StorageDevice { kind, backend_name, pci_path } = device;
        api::StorageDevice {
            kind: kind.into(),
            backend_name,
            pci_path: pci_path.into(),
        }
    }
}

impl From<StorageDeviceKind> for api::StorageDeviceKind {
    fn from(kind: StorageDeviceKind) -> Self {
        match kind {
            StorageDeviceKind::Virtio => api::StorageDeviceKind::Virtio,
            StorageDeviceKind::Nvme => api::StorageDeviceKind::Nvme,
        }
    }
}

impl From<NetworkDevice> for api::NetworkDevice {
    fn from(device: NetworkDevice) -> Self {
        let NetworkDevice { backend_name, pci_path } = device;
        api::NetworkDevice { backend_name, pci_path: pci_path.into() }
    }
}

impl From<PciPciBridge> for api::PciPciBridge {
    fn from(bridge: PciPciBridge) -> Self {
        let PciPciBridge { downstream_bus, pci_path } = bridge;
        api::PciPciBridge { downstream_bus, pci_path: pci_path.into() }
    }
}

impl From<SerialPort> for api::SerialPort {
    fn from(port: SerialPort) -> Self {
        api::SerialPort { num: port.num.into() }
    }
}

impl From<SerialPortNumber> for api::SerialPortNumber {
    fn from(num: SerialPortNumber) -> Self {
        match num {
            SerialPortNumber::Com1 => api::SerialPortNumber::Com1,
            SerialPortNumber::Com2 => api::SerialPortNumber::Com2,
            SerialPortNumber::Com3 => api::SerialPortNumber::Com3,
            SerialPortNumber::Com4 => api::SerialPortNumber::Com4,
        }
    }
}

impl From<PciPath> for api::PciPath {
    fn from(path: PciPath) -> Self {
        path.into()
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
            CrucibleVCR::Region { block_size, opts, gen } => {
                GenVCR::Region { block_size, opts: opts.into(), gen }
            }
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
            flush_timeout,
            key,
            cert_pem,
            key_pem,
            root_cert_pem,
            control: control.map(|c| c.to_string()),
            read_only,
        }
    }
}
