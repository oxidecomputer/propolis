// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from version-0 instance specs in the [`propolis_api_types`]
//! crate to the internal [`super::Spec`] representation.

use std::collections::HashMap;

use propolis_api_types::instance_spec::{
    components::devices::SerialPort as SerialPortDesc,
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
        // Inserts a component entry into the supplied map, asserting first that
        // the supplied key is not present in that map.
        //
        // This assertion is valid because internal instance specs should assign
        // a unique name to each component they describe. The spec builder
        // upholds this invariant at spec creation time.
        #[track_caller]
        fn insert_component<T>(
            map: &mut HashMap<String, T>,
            key: String,
            val: T,
        ) {
            assert!(
                !map.contains_key(&key),
                "component name {} already exists in output spec",
                &key
            );
            map.insert(key, val);
        }

        let mut spec = InstanceSpecV0::default();
        spec.devices.board = val.board;
        for (disk_name, disk) in val.disks {
            let backend_name = disk.device_spec.backend_name().to_owned();
            insert_component(
                &mut spec.devices.storage_devices,
                disk_name,
                disk.device_spec.into(),
            );

            insert_component(
                &mut spec.backends.storage_backends,
                backend_name,
                disk.backend_spec.into(),
            );
        }

        for (nic_name, nic) in val.nics {
            let backend_name = nic.device_spec.backend_name.clone();
            insert_component(
                &mut spec.devices.network_devices,
                nic_name,
                NetworkDeviceV0::VirtioNic(nic.device_spec),
            );

            insert_component(
                &mut spec.backends.network_backends,
                backend_name,
                NetworkBackendV0::Virtio(nic.backend_spec),
            );
        }

        for (name, desc) in val.serial {
            if desc.device == SerialPortDevice::Uart {
                insert_component(
                    &mut spec.devices.serial_ports,
                    name,
                    SerialPortDesc { num: desc.num },
                );
            }
        }

        for (bridge_name, bridge) in val.pci_pci_bridges {
            insert_component(
                &mut spec.devices.pci_pci_bridges,
                bridge_name,
                bridge,
            );
        }

        spec.devices.qemu_pvpanic = val.pvpanic.map(|pvpanic| pvpanic.spec);

        #[cfg(feature = "falcon")]
        {
            spec.devices.softnpu_pci_port = val.softnpu.pci_port;
            spec.devices.softnpu_p9 = val.softnpu.p9_device;
            spec.devices.p9fs = val.softnpu.p9fs;
            for (port_name, port) in val.softnpu.ports {
                insert_component(
                    &mut spec.devices.softnpu_ports,
                    port_name.clone(),
                    SoftNpuPortSpec {
                        name: port_name,
                        backend_name: port.backend_name.clone(),
                    },
                );

                insert_component(
                    &mut spec.backends.network_backends,
                    port.backend_name,
                    NetworkBackendV0::Dlpi(port.backend_spec),
                );
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

            let (_, backend_spec) = value
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
            let (_, backend_spec) = value
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
                Nic { device_spec, backend_spec },
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

        for (name, serial_port) in value.devices.serial_ports {
            builder.add_serial_port(name, serial_port.num)?;
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
