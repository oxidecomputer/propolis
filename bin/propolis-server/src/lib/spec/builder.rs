// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A builder for instance specs.

use std::collections::{BTreeSet, HashSet};

use cpuid_utils::CpuidMapConversionError;
use propolis_api_types::instance_spec::{
    components::{
        board::{Board as InstanceSpecBoard, Chipset, I440Fx},
        devices::{PciPciBridge, SerialPortNumber},
    },
    PciPath, SpecKey,
};
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::devices::{
    P9fs, SoftNpuP9, SoftNpuPciPort,
};

use crate::{config, spec::SerialPortDevice};

use super::{
    config_toml::{ConfigTomlError, ParsedConfig},
    Board, BootOrderEntry, BootSettings, Disk, Nic, QemuPvpanic, SerialPort,
};

#[cfg(feature = "falcon")]
use super::{ParsedSoftNpu, SoftNpuPort};

/// Errors that can arise while building an instance spec from component parts.
#[derive(Debug, Error)]
pub(crate) enum SpecBuilderError {
    #[error("error parsing config TOML")]
    ConfigToml(#[from] ConfigTomlError),

    #[error("device {0} has the same ID as its backend")]
    DeviceAndBackendNamesIdentical(SpecKey),

    #[error("a component with ID {0} already exists")]
    ComponentIdInUse(SpecKey),

    #[error("a PCI device is already attached at {0:?}")]
    PciPathInUse(PciPath),

    #[error("serial port {0:?} is already specified")]
    SerialPortInUse(SerialPortNumber),

    #[error("pvpanic device already specified")]
    PvpanicInUse,

    #[error("boot settings were already specified")]
    BootSettingsInUse,

    #[error("boot option {0} is not an attached device")]
    BootOptionMissing(SpecKey),

    #[error("instance spec's CPUID entries are invalid")]
    CpuidEntriesInvalid(#[from] cpuid_utils::CpuidMapConversionError),
}

#[derive(Debug, Default)]
pub(crate) struct SpecBuilder {
    spec: super::Spec,
    pci_paths: BTreeSet<PciPath>,
    serial_ports: HashSet<SerialPortNumber>,
    component_ids: BTreeSet<SpecKey>,
}

impl SpecBuilder {
    pub fn new(cpus: u8, memory_mb: u64) -> Self {
        let board = Board {
            cpus,
            memory_mb,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
        };

        Self {
            spec: super::Spec { board, ..Default::default() },
            ..Default::default()
        }
    }

    pub(super) fn with_instance_spec_board(
        board: InstanceSpecBoard,
    ) -> Result<Self, SpecBuilderError> {
        Ok(Self {
            spec: super::Spec {
                board: Board {
                    cpus: board.cpus,
                    memory_mb: board.memory_mb,
                    chipset: board.chipset,
                },
                cpuid: board
                    .cpuid
                    .map(|cpuid| -> Result<_, CpuidMapConversionError> {
                        {
                            Ok(cpuid_utils::CpuidSet::from_map(
                                cpuid.entries.try_into()?,
                                cpuid.vendor,
                            )?)
                        }
                    })
                    .transpose()?,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// Sets the spec's boot order to the list of disk devices specified in
    /// `boot_options`.
    ///
    /// All of the items in the supplied `boot_options` must already be present
    /// in the spec's disk map.
    pub fn add_boot_order(
        &mut self,
        component_id: SpecKey,
        boot_options: impl Iterator<Item = BootOrderEntry>,
    ) -> Result<(), SpecBuilderError> {
        if self.component_ids.contains(&component_id) {
            return Err(SpecBuilderError::ComponentIdInUse(component_id));
        }

        if self.spec.boot_settings.is_some() {
            return Err(SpecBuilderError::BootSettingsInUse);
        }

        let mut order = vec![];
        for item in boot_options {
            if !self.spec.disks.contains_key(&item.component_id) {
                return Err(SpecBuilderError::BootOptionMissing(
                    item.component_id,
                ));
            }

            order.push(crate::spec::BootOrderEntry {
                component_id: item.component_id,
            });
        }

        self.spec.boot_settings = Some(BootSettings { component_id, order });
        Ok(())
    }

    /// Adds all the devices and backends specified in the supplied
    /// configuration TOML to the spec under construction.
    pub fn add_devices_from_config(
        &mut self,
        config: &config::Config,
    ) -> Result<(), SpecBuilderError> {
        let parsed = ParsedConfig::try_from(config)?;

        let Chipset::I440Fx(ref mut i440fx) = self.spec.board.chipset;
        i440fx.enable_pcie = parsed.enable_pcie;

        for disk in parsed.disks {
            self.add_storage_device(disk.id, disk.disk)?;
        }

        for nic in parsed.nics {
            self.add_network_device(nic.id, nic.nic)?;
        }

        for bridge in parsed.pci_bridges {
            self.add_pci_bridge(bridge.id, bridge.bridge)?;
        }

        #[cfg(feature = "falcon")]
        self.add_parsed_softnpu_devices(parsed.softnpu)?;

        Ok(())
    }

    #[cfg(feature = "falcon")]
    fn add_parsed_softnpu_devices(
        &mut self,
        devices: ParsedSoftNpu,
    ) -> Result<(), SpecBuilderError> {
        if let Some(pci_port) = devices.pci_port {
            self.set_softnpu_pci_port(pci_port)?;
        }

        for port in devices.ports {
            self.add_softnpu_port(port.id, port.port)?;
        }

        if let Some(p9) = devices.p9_device {
            self.set_softnpu_p9(p9)?;
        }

        if let Some(p9fs) = devices.p9fs {
            self.set_p9fs(p9fs)?;
        }

        Ok(())
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
        disk_id: SpecKey,
        disk: Disk,
    ) -> Result<&Self, SpecBuilderError> {
        if disk_id == *disk.device_spec.backend_id() {
            return Err(SpecBuilderError::DeviceAndBackendNamesIdentical(
                disk_id,
            ));
        }

        if self.component_ids.contains(&disk_id) {
            return Err(SpecBuilderError::ComponentIdInUse(disk_id));
        }

        if self.component_ids.contains(disk.device_spec.backend_id()) {
            return Err(SpecBuilderError::ComponentIdInUse(
                disk.device_spec.backend_id().to_owned(),
            ));
        }

        self.register_pci_device(disk.device_spec.pci_path())?;
        self.component_ids.insert(disk_id.clone());
        self.component_ids.insert(disk.device_spec.backend_id().to_owned());
        let _old = self.spec.disks.insert(disk_id, disk);
        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a network device with an associated backend.
    pub(super) fn add_network_device(
        &mut self,
        nic_id: SpecKey,
        nic: Nic,
    ) -> Result<&Self, SpecBuilderError> {
        if nic_id == nic.device_spec.backend_id {
            return Err(SpecBuilderError::DeviceAndBackendNamesIdentical(
                nic_id,
            ));
        }

        if self.component_ids.contains(&nic_id) {
            return Err(SpecBuilderError::ComponentIdInUse(nic_id));
        }

        if self.component_ids.contains(&nic.device_spec.backend_id) {
            return Err(SpecBuilderError::ComponentIdInUse(
                nic.device_spec.backend_id,
            ));
        }

        self.register_pci_device(nic.device_spec.pci_path)?;
        self.component_ids.insert(nic_id.clone());
        self.component_ids.insert(nic.device_spec.backend_id.clone());
        let _old = self.spec.nics.insert(nic_id, nic);
        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a PCI-PCI bridge.
    pub fn add_pci_bridge(
        &mut self,
        id: SpecKey,
        bridge: PciPciBridge,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_ids.contains(&id) {
            return Err(SpecBuilderError::ComponentIdInUse(id));
        }

        self.register_pci_device(bridge.pci_path)?;
        self.component_ids.insert(id.clone());
        let _old = self.spec.pci_pci_bridges.insert(id, bridge);
        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a serial port.
    pub fn add_serial_port(
        &mut self,
        id: SpecKey,
        num: SerialPortNumber,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_ids.contains(&id) {
            return Err(SpecBuilderError::ComponentIdInUse(id));
        }

        if self.serial_ports.contains(&num) {
            return Err(SpecBuilderError::SerialPortInUse(num));
        }

        let desc = SerialPort { num, device: SerialPortDevice::Uart };
        self.spec.serial.insert(id.clone(), desc);
        self.component_ids.insert(id);
        self.serial_ports.insert(num);
        Ok(self)
    }

    pub fn add_pvpanic_device(
        &mut self,
        pvpanic: QemuPvpanic,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_ids.contains(&pvpanic.id) {
            return Err(SpecBuilderError::ComponentIdInUse(pvpanic.id));
        }

        if self.spec.pvpanic.is_some() {
            return Err(SpecBuilderError::PvpanicInUse);
        }

        self.component_ids.insert(pvpanic.id.clone());
        self.spec.pvpanic = Some(pvpanic);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_softnpu_com4(
        &mut self,
        id: SpecKey,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_ids.contains(&id) {
            return Err(SpecBuilderError::ComponentIdInUse(id));
        }

        let num = SerialPortNumber::Com4;
        if self.serial_ports.contains(&num) {
            return Err(SpecBuilderError::SerialPortInUse(num));
        }

        let desc = SerialPort { num, device: SerialPortDevice::SoftNpu };
        self.spec.serial.insert(id.clone(), desc);
        self.component_ids.insert(id);
        self.serial_ports.insert(num);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_softnpu_pci_port(
        &mut self,
        pci_port: SoftNpuPciPort,
    ) -> Result<&Self, SpecBuilderError> {
        self.register_pci_device(pci_port.pci_path)?;
        self.spec.softnpu.pci_port = Some(pci_port);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_softnpu_p9(
        &mut self,
        p9: SoftNpuP9,
    ) -> Result<&Self, SpecBuilderError> {
        self.register_pci_device(p9.pci_path)?;
        self.spec.softnpu.p9_device = Some(p9);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_p9fs(&mut self, p9fs: P9fs) -> Result<&Self, SpecBuilderError> {
        self.register_pci_device(p9fs.pci_path)?;
        self.spec.softnpu.p9fs = Some(p9fs);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn add_softnpu_port(
        &mut self,
        port_id: SpecKey,
        port: SoftNpuPort,
    ) -> Result<&Self, SpecBuilderError> {
        if port_id == port.backend_id {
            return Err(SpecBuilderError::DeviceAndBackendNamesIdentical(
                port_id,
            ));
        }

        if self.component_ids.contains(&port_id) {
            return Err(SpecBuilderError::ComponentIdInUse(port_id));
        }

        if self.component_ids.contains(&port.backend_id) {
            return Err(SpecBuilderError::ComponentIdInUse(port.backend_id));
        }

        let _old = self.spec.softnpu.ports.insert(port_id, port);
        assert!(_old.is_none());
        Ok(self)
    }

    /// Yields the completed spec, consuming the builder.
    pub fn finish(self) -> super::Spec {
        self.spec
    }
}
