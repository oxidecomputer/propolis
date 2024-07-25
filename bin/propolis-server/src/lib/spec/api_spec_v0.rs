// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from version-0 instance specs in the [`propolis_api_types`]
//! crate to the internal [`super::Spec`] representation.

use propolis_api_types::instance_spec::{
    components::devices::{SerialPort as SerialPortSpec, SerialPortNumber},
    v0::{InstanceSpecV0, NetworkDeviceV0, StorageDeviceV0},
};
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::{
    components::devices::SoftNpuPort as SoftNpuPortSpec, v0::NetworkBackendV0,
};

#[cfg(feature = "falcon")]
use crate::spec::SoftNpuPort;

use super::{
    builder::{SpecBuilder, SpecBuilderError},
    Disk, Nic, PciPciBridge, QemuPvpanic, SerialPort, Spec,
};

#[derive(Debug, Error)]
pub(crate) enum ApiSpecParseError {
    #[error(transparent)]
    BuilderError(#[from] SpecBuilderError),

    #[error("backend {0} not found for device {1}")]
    BackendNotFound(String, String),

    #[error("backend {0} not used by any device")]
    BackendNotUsed(String),

    #[cfg(feature = "falcon")]
    #[error("network backend for device {0} is not a DLPI backend")]
    NotDlpiBackend(String),
}

impl From<Spec> for InstanceSpecV0 {
    fn from(val: Spec) -> Self {
        let mut spec = InstanceSpecV0::default();

        spec.devices.board = val.board;
        for disk in val.disks {
            let _old = spec
                .devices
                .storage_devices
                .insert(disk.device_name, disk.device_spec);

            assert!(_old.is_none());

            let _old = spec
                .backends
                .storage_backends
                .insert(disk.backend_name, disk.backend_spec);

            assert!(_old.is_none());
        }

        for nic in val.nics {
            let _old = spec
                .devices
                .network_devices
                .insert(nic.device_name, nic.device_spec);

            assert!(_old.is_none());

            let _old = spec
                .backends
                .network_backends
                .insert(nic.backend_name, nic.backend_spec);

            assert!(_old.is_none());
        }

        for (index, serial) in val.serial.iter().enumerate() {
            if *serial == SerialPort::Enabled {
                let _old = spec.devices.serial_ports.insert(
                    format!("com{}", index + 1),
                    SerialPortSpec {
                        num: match index {
                            0 => SerialPortNumber::Com1,
                            1 => SerialPortNumber::Com2,
                            2 => SerialPortNumber::Com3,
                            3 => SerialPortNumber::Com4,
                            _ => unreachable!(),
                        },
                    },
                );

                assert!(_old.is_none());
            }
        }

        for bridge in val.pci_pci_bridges {
            let _old =
                spec.devices.pci_pci_bridges.insert(bridge.name(), bridge.0);

            assert!(_old.is_none());
        }

        spec.devices.qemu_pvpanic = val.pvpanic.map(|pvpanic| pvpanic.0);

        #[cfg(feature = "falcon")]
        {
            spec.devices.softnpu_pci_port = val.softnpu.pci_port;
            spec.devices.softnpu_p9 = val.softnpu.p9_device;
            spec.devices.p9fs = val.softnpu.p9fs;
            for port in val.softnpu.ports {
                let _old = spec.devices.softnpu_ports.insert(
                    port.name.clone(),
                    SoftNpuPortSpec {
                        name: port.name,
                        backend_name: port.backend_name.clone(),
                    },
                );

                assert!(_old.is_none());

                let _old = spec.backends.network_backends.insert(
                    port.backend_name,
                    NetworkBackendV0::Dlpi(port.backend_spec),
                );

                assert!(_old.is_none());
            }
        }

        spec
    }
}

impl TryFrom<InstanceSpecV0> for Spec {
    type Error = ApiSpecParseError;

    fn try_from(mut value: InstanceSpecV0) -> Result<Self, Self::Error> {
        let mut builder = SpecBuilder::with_board(value.devices.board);
        for (device_name, device_spec) in value.devices.storage_devices {
            let backend_name = match &device_spec {
                StorageDeviceV0::VirtioDisk(disk) => &disk.backend_name,
                StorageDeviceV0::NvmeDisk(disk) => &disk.backend_name,
            };

            let (backend_name, backend_spec) = value
                .backends
                .storage_backends
                .remove_entry(backend_name)
                .ok_or_else(|| {
                    ApiSpecParseError::BackendNotFound(
                        backend_name.to_owned(),
                        device_name.clone(),
                    )
                })?;

            builder.add_storage_device(Disk {
                device_name,
                device_spec,
                backend_name,
                backend_spec,
            })?;
        }

        if let Some(backend) = value.backends.storage_backends.keys().next() {
            return Err(ApiSpecParseError::BackendNotUsed(backend.to_owned()));
        }

        for (device_name, device_spec) in value.devices.network_devices {
            let backend_name = match &device_spec {
                NetworkDeviceV0::VirtioNic(nic) => &nic.backend_name,
            };

            let (backend_name, backend_spec) = value
                .backends
                .network_backends
                .remove_entry(backend_name)
                .ok_or_else(|| {
                    ApiSpecParseError::BackendNotFound(
                        backend_name.to_owned(),
                        device_name.clone(),
                    )
                })?;

            builder.add_network_device(Nic {
                device_name,
                device_spec,
                backend_name,
                backend_spec,
            })?;
        }

        // SoftNPU ports can have network backends, so consume the SoftNPU
        // device fields before checking to see if the network backend list is
        // empty.
        #[cfg(feature = "falcon")]
        {
            if let Some(softnpu_pci) = value.devices.softnpu_pci_port {
                builder.set_softnpu_pci_port(softnpu_pci)?;
            }

            if let Some(softnpu_p9) = value.devices.softnpu_p9 {
                builder.set_softnpu_p9(softnpu_p9)?;
            }

            if let Some(p9fs) = value.devices.p9fs {
                builder.set_p9fs(p9fs)?;
            }

            for (port_name, port) in value.devices.softnpu_ports {
                let (backend_name, backend_spec) = value
                    .backends
                    .network_backends
                    .remove_entry(&port.backend_name)
                    .ok_or_else(|| {
                        ApiSpecParseError::BackendNotFound(
                            port.backend_name,
                            port_name.clone(),
                        )
                    })?;

                let NetworkBackendV0::Dlpi(backend_spec) = backend_spec else {
                    return Err(ApiSpecParseError::NotDlpiBackend(port_name));
                };

                builder.add_softnpu_port(SoftNpuPort {
                    name: port_name,
                    backend_name,
                    backend_spec,
                })?;
            }
        }

        if let Some(backend) = value.backends.network_backends.keys().next() {
            return Err(ApiSpecParseError::BackendNotUsed(backend.to_owned()));
        }

        // TODO(gjc) do more to preserve names
        for serial_port in value.devices.serial_ports.values() {
            builder.add_serial_port(serial_port.num)?;
        }

        // TODO(gjc) do more to preserve names
        for bridge in value.devices.pci_pci_bridges.into_values() {
            builder.add_pci_bridge(PciPciBridge(bridge))?;
        }

        if let Some(pvpanic) = value.devices.qemu_pvpanic {
            builder.add_pvpanic_device(QemuPvpanic(pvpanic))?;
        }

        Ok(builder.finish())
    }
}
