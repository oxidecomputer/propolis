// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A builder for instance specs.

use std::collections::BTreeSet;

use propolis_api_types::instance_spec::{
    components::{
        board::Board,
        devices::{PciPciBridge, QemuPvpanic, SerialPort, SerialPortNumber},
    },
    v0::{DeviceSpecV0, InstanceSpecV0, NetworkDeviceV0, StorageDeviceV0},
    PciPath,
};
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::{
    components::{
        backends::DlpiNetworkBackend,
        devices::{P9fs, SoftNpuP9, SoftNpuPciPort, SoftNpuPort},
    },
    v0::NetworkBackendV0,
};

use super::{ParsedNetworkDevice, ParsedStorageDevice};

/// Errors that can arise while building an instance spec from component parts.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Error)]
pub(crate) enum SpecBuilderError {
    #[error("A device with name {0} already exists")]
    DeviceNameInUse(String),

    #[error("A backend with name {0} already exists")]
    BackendNameInUse(String),

    #[error("A PCI device is already attached at {0:?}")]
    PciPathInUse(PciPath),

    #[error("Serial port {0:?} is already specified")]
    SerialPortInUse(SerialPortNumber),

    #[cfg(feature = "falcon")]
    #[error("SoftNpu port {0:?} is already specified")]
    SoftNpuPortInUse(String),
}

pub(crate) struct SpecBuilder {
    spec: InstanceSpecV0,
    pci_paths: BTreeSet<PciPath>,
}

trait PciComponent {
    fn pci_path(&self) -> PciPath;
}

impl PciComponent for StorageDeviceV0 {
    fn pci_path(&self) -> PciPath {
        match self {
            StorageDeviceV0::VirtioDisk(disk) => disk.pci_path,
            StorageDeviceV0::NvmeDisk(disk) => disk.pci_path,
        }
    }
}

impl PciComponent for NetworkDeviceV0 {
    fn pci_path(&self) -> PciPath {
        match self {
            NetworkDeviceV0::VirtioNic(nic) => nic.pci_path,
        }
    }
}

impl SpecBuilder {
    pub(super) fn new(board: Board) -> Self {
        Self {
            spec: InstanceSpecV0 {
                devices: DeviceSpecV0 { board, ..Default::default() },
                backends: Default::default(),
            },
            pci_paths: Default::default(),
        }
    }

    /// Adds a PCI path to this builder's record of PCI locations with an
    /// attached device. If the path is already in use, returns an error.
    fn register_pci_device(
        &mut self,
        pci_path: PciPath,
    ) -> Result<(), SpecBuilderError> {
        if self.pci_paths.contains(&pci_path) {
            Err(SpecBuilderError::PciPathInUse(pci_path))
        } else {
            self.pci_paths.insert(pci_path);
            Ok(())
        }
    }

    /// Adds a storage device with an associated backend.
    pub(super) fn add_storage_device(
        &mut self,
        ParsedStorageDevice {
            device_name,
            device_spec,
            backend_name,
            backend_spec,
        }: ParsedStorageDevice,
    ) -> Result<&Self, SpecBuilderError> {
        if self.spec.devices.storage_devices.contains_key(&device_name) {
            return Err(SpecBuilderError::DeviceNameInUse(device_name));
        }

        if self.spec.backends.storage_backends.contains_key(&backend_name) {
            return Err(SpecBuilderError::BackendNameInUse(backend_name));
        }
        self.register_pci_device(device_spec.pci_path())?;
        let _old =
            self.spec.devices.storage_devices.insert(device_name, device_spec);

        assert!(_old.is_none());
        let _old = self
            .spec
            .backends
            .storage_backends
            .insert(backend_name, backend_spec);

        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a network device with an associated backend.
    pub(super) fn add_network_device(
        &mut self,
        ParsedNetworkDevice {
            device_name,
            device_spec,
            backend_name,
            backend_spec,
        }: ParsedNetworkDevice,
    ) -> Result<&Self, SpecBuilderError> {
        if self.spec.devices.network_devices.contains_key(&device_name) {
            return Err(SpecBuilderError::DeviceNameInUse(device_name));
        }

        if self.spec.backends.network_backends.contains_key(&backend_name) {
            return Err(SpecBuilderError::BackendNameInUse(backend_name));
        }

        self.register_pci_device(device_spec.pci_path())?;
        let _old =
            self.spec.devices.network_devices.insert(device_name, device_spec);

        assert!(_old.is_none());
        let _old = self
            .spec
            .backends
            .network_backends
            .insert(backend_name, backend_spec);

        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a PCI-PCI bridge.
    pub fn add_pci_bridge(
        &mut self,
        bridge_name: String,
        bridge_spec: PciPciBridge,
    ) -> Result<&Self, SpecBuilderError> {
        if self.spec.devices.pci_pci_bridges.contains_key(&bridge_name) {
            return Err(SpecBuilderError::DeviceNameInUse(bridge_name));
        }

        self.register_pci_device(bridge_spec.pci_path)?;
        let _old =
            self.spec.devices.pci_pci_bridges.insert(bridge_name, bridge_spec);

        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a serial port.
    pub fn add_serial_port(
        &mut self,
        port: SerialPortNumber,
    ) -> Result<&Self, SpecBuilderError> {
        if self
            .spec
            .devices
            .serial_ports
            .insert(
                match port {
                    SerialPortNumber::Com1 => "com1",
                    SerialPortNumber::Com2 => "com2",
                    SerialPortNumber::Com3 => "com3",
                    SerialPortNumber::Com4 => "com4",
                }
                .to_string(),
                SerialPort { num: port },
            )
            .is_some()
        {
            Err(SpecBuilderError::SerialPortInUse(port))
        } else {
            Ok(self)
        }
    }

    pub fn add_pvpanic_device(
        &mut self,
        pvpanic: QemuPvpanic,
    ) -> Result<&Self, SpecBuilderError> {
        if self.spec.devices.qemu_pvpanic.is_some() {
            return Err(SpecBuilderError::DeviceNameInUse(
                "pvpanic".to_string(),
            ));
        }

        self.spec.devices.qemu_pvpanic = Some(pvpanic);

        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_softnpu_pci_port(
        &mut self,
        pci_port: SoftNpuPciPort,
    ) -> Result<&Self, SpecBuilderError> {
        self.register_pci_device(pci_port.pci_path)?;
        self.spec.devices.softnpu_pci_port = Some(pci_port);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_softnpu_p9(
        &mut self,
        p9: SoftNpuP9,
    ) -> Result<&Self, SpecBuilderError> {
        self.register_pci_device(p9.pci_path)?;
        self.spec.devices.softnpu_p9 = Some(p9);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_p9fs(&mut self, p9fs: P9fs) -> Result<&Self, SpecBuilderError> {
        self.register_pci_device(p9fs.pci_path)?;
        self.spec.devices.p9fs = Some(p9fs);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn add_softnpu_port(
        &mut self,
        key: String,
        port: SoftNpuPort,
    ) -> Result<&Self, SpecBuilderError> {
        let _old = self.spec.backends.network_backends.insert(
            port.backend_name.clone(),
            NetworkBackendV0::Dlpi(DlpiNetworkBackend {
                vnic_name: port.backend_name.clone(),
            }),
        );
        assert!(_old.is_none());
        if self.spec.devices.softnpu_ports.insert(key, port.clone()).is_some() {
            Err(SpecBuilderError::SoftNpuPortInUse(port.name))
        } else {
            Ok(self)
        }
    }

    /// Yields the completed spec, consuming the builder.
    pub fn finish(self) -> InstanceSpecV0 {
        self.spec
    }
}
