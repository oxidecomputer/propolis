// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from version-0 instance specs in the [`propolis_api_types`]
//! crate to the internal [`super::Spec`] representation.

use propolis_api_types::instance_spec::{
    components::devices::{SerialPort as SerialPortDesc, SerialPortNumber},
    v0::{InstanceSpecV0, NetworkBackendV0, NetworkDeviceV0, StorageDeviceV0},
};
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::devices::SoftNpuPort as SoftNpuPortSpec;

#[cfg(feature = "falcon")]
use crate::spec::SoftNpuPort;

use super::{
    builder::{SpecBuilder, SpecBuilderError},
    Disk, Nic, QemuPvpanic, SerialPortDevice, Spec,
};

#[derive(Debug, Error)]
pub(crate) enum ApiSpecError {
    #[error(transparent)]
    Builder(#[from] SpecBuilderError),

    #[error("backend {backend} not found for device {device}")]
    BackendNotFound { backend: String, device: String },

    #[error("backend {0} not used by any device")]
    BackendNotUsed(String),

    #[error("network backend for guest NIC {0} is not a viona backend")]
    GuestNicInvalidBackend(String),

    #[cfg(feature = "falcon")]
    #[error("network backend for device {0} is not a DLPI backend")]
    NotDlpiBackend(String),
}

impl From<Spec> for InstanceSpecV0 {
    fn from(val: Spec) -> Self {
        let mut spec = InstanceSpecV0::default();
        spec.devices.board = val.board;

        // Internal instance specs (the inputs to this function) should assign
        // a unique name to each component they describe. This invariant is
        // enforced by the spec builder. Since component names are globally
        // unique, they should never collide when inserted into an API spec's
        // device and backend maps.
        for (disk_name, disk) in val.disks {
            let old = spec
                .devices
                .storage_devices
                .insert(disk_name, disk.device_spec.into());

            assert!(old.is_none(), "{old:?}");

            let old = spec
                .backends
                .storage_backends
                .insert(disk.backend_name, disk.backend_spec.into());

            assert!(old.is_none(), "{old:?}");
        }

        for (nic_name, nic) in val.nics {
            let old = spec
                .devices
                .network_devices
                .insert(nic_name, NetworkDeviceV0::VirtioNic(nic.device_spec));

            assert!(old.is_none(), "{old:?}");

            let old = spec.backends.network_backends.insert(
                nic.backend_name,
                NetworkBackendV0::Virtio(nic.backend_spec),
            );

            assert!(old.is_none(), "{old:?}");
        }

        for (num, user) in val.serial.iter() {
            if *user == SerialPortDevice::Uart {
                let name = match num {
                    SerialPortNumber::Com1 => "com1",
                    SerialPortNumber::Com2 => "com2",
                    SerialPortNumber::Com3 => "com3",
                    SerialPortNumber::Com4 => "com4",
                };

                let old = spec
                    .devices
                    .serial_ports
                    .insert(name.to_owned(), SerialPortDesc { num: *num });

                assert!(old.is_none(), "{old:?}");
            }
        }

        for (bridge_name, bridge) in val.pci_pci_bridges {
            let old = spec.devices.pci_pci_bridges.insert(bridge_name, bridge);
            assert!(old.is_none(), "{old:?}");
        }

        spec.devices.qemu_pvpanic = val.pvpanic.map(|pvpanic| pvpanic.spec);

        #[cfg(feature = "falcon")]
        {
            spec.devices.softnpu_pci_port = val.softnpu.pci_port;
            spec.devices.softnpu_p9 = val.softnpu.p9_device;
            spec.devices.p9fs = val.softnpu.p9fs;
            for (port_name, port) in val.softnpu.ports {
                let old = spec.devices.softnpu_ports.insert(
                    port_name.clone(),
                    SoftNpuPortSpec {
                        name: port_name,
                        backend_name: port.backend_name.clone(),
                    },
                );

                assert!(old.is_none(), "{old:?}");

                let old = spec.backends.network_backends.insert(
                    port.backend_name,
                    NetworkBackendV0::Dlpi(port.backend_spec),
                );

                assert!(old.is_none(), "{old:?}");
            }
        }

        spec
    }
}

impl TryFrom<InstanceSpecV0> for Spec {
    type Error = ApiSpecError;

    fn try_from(mut value: InstanceSpecV0) -> Result<Self, Self::Error> {
        let mut builder = SpecBuilder::with_board(value.devices.board);

        // Examine each storage device and peel its backend off of the input
        // spec.
        for (device_name, device_spec) in value.devices.storage_devices {
            let backend_name = match &device_spec {
                StorageDeviceV0::VirtioDisk(disk) => &disk.backend_name,
                StorageDeviceV0::NvmeDisk(disk) => &disk.backend_name,
            };

            let (backend_name, backend_spec) = value
                .backends
                .storage_backends
                .remove_entry(backend_name)
                .ok_or_else(|| ApiSpecError::BackendNotFound {
                    backend: backend_name.to_owned(),
                    device: device_name.clone(),
                })?;

            builder.add_storage_device(
                device_name,
                Disk {
                    device_spec: device_spec.into(),
                    backend_name,
                    backend_spec: backend_spec.into(),
                },
            )?;
        }

        // Once all the devices have been checked, there should be no unpaired
        // backends remaining.
        if let Some(backend) = value.backends.storage_backends.keys().next() {
            return Err(ApiSpecError::BackendNotUsed(backend.to_owned()));
        }

        // Repeat this process for network devices.
        for (device_name, device_spec) in value.devices.network_devices {
            let NetworkDeviceV0::VirtioNic(device_spec) = device_spec;
            let backend_name = &device_spec.backend_name;
            let (backend_name, backend_spec) = value
                .backends
                .network_backends
                .remove_entry(backend_name)
                .ok_or_else(|| ApiSpecError::BackendNotFound {
                    backend: backend_name.to_owned(),
                    device: device_name.clone(),
                })?;

            let NetworkBackendV0::Virtio(backend_spec) = backend_spec else {
                return Err(ApiSpecError::GuestNicInvalidBackend(device_name));
            };

            builder.add_network_device(
                device_name,
                Nic { device_spec, backend_name, backend_spec },
            )?;
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
                    .ok_or_else(|| ApiSpecError::BackendNotFound {
                        backend: port.backend_name,
                        device: port_name.clone(),
                    })?;

                let NetworkBackendV0::Dlpi(backend_spec) = backend_spec else {
                    return Err(ApiSpecError::NotDlpiBackend(port_name));
                };

                builder.add_softnpu_port(
                    port_name,
                    SoftNpuPort { backend_name, backend_spec },
                )?;
            }
        }

        if let Some(backend) = value.backends.network_backends.keys().next() {
            return Err(ApiSpecError::BackendNotUsed(backend.to_owned()));
        }

        // TODO(#735): Serial ports need to have names like other devices.
        for serial_port in value.devices.serial_ports.values() {
            builder.add_serial_port(serial_port.num)?;
        }

        for (name, bridge) in value.devices.pci_pci_bridges {
            builder.add_pci_bridge(name, bridge)?;
        }

        if let Some(pvpanic) = value.devices.qemu_pvpanic {
            builder.add_pvpanic_device(QemuPvpanic {
                name: "pvpanic".to_string(),
                spec: pvpanic,
            })?;
        }

        Ok(builder.finish())
    }
}
