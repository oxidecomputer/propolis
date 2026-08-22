// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from types in [`propolis_api_types`] - that is, the latest
//! Propolis API version - to the internal [`super::Spec`] representation.
//!
//! `propolis_api_types` is a re-export of the latest versions of types out of
//! `propolis_api_types_versions`. The types that tend to change across files
//! are referred to here as `latest::<the-item>` for similarity to other
//! version-specific code. Types that do not tend to change (say, `SpecKey`) are
//! just taken from the re-exported path because that's how they're used
//! everywhere (including other `api_spec_*` files.)

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::{
    components::backends::{DlpiNetworkBackend, VirtioNetworkBackend},
    SpecKey,
};
use propolis_api_types_versions::latest;

use super::{
    builder::SpecBuilder, ApiSpecError, Disk, Nic, QemuPvpanic, StorageBackend,
    StorageDevice,
};

#[cfg(feature = "failure-injection")]
use super::MigrationFailure;

#[cfg(feature = "falcon")]
use super::SoftNpuPort;

/// Parses the latest form of `InstanceSpec` into a [`SpecBuilder`], validating
/// component names, PCI paths, and backend references along the way. Callers
/// can add additional components to the builder before calling `finish()`.
pub(crate) fn latest_to_spec_builder(
    value: latest::instance_spec::InstanceSpec,
) -> Result<SpecBuilder, ApiSpecError> {
    let mut builder = SpecBuilder::with_instance_spec_board(value.board)?;

    if let Some(smbios) = value.smbios {
        builder.set_smbios_type1_input(smbios);
    }

    let mut devices: Vec<(SpecKey, latest::instance_spec::Component)> = vec![];
    let mut boot_settings = None;
    let mut storage_backends: BTreeMap<SpecKey, StorageBackend> =
        BTreeMap::new();
    let mut viona_backends: BTreeMap<SpecKey, VirtioNetworkBackend> =
        BTreeMap::new();
    let mut dlpi_backends: BTreeMap<SpecKey, DlpiNetworkBackend> =
        BTreeMap::new();

    for (id, component) in value.components.into_iter() {
        match component {
            latest::instance_spec::Component::CrucibleStorageBackend(_)
            | latest::instance_spec::Component::FileStorageBackend(_)
            | latest::instance_spec::Component::BlobStorageBackend(_) => {
                storage_backends.insert(
                    id,
                    component
                        .try_into()
                        .expect("component is known to be a storage backend"),
                );
            }
            latest::instance_spec::Component::VirtioNetworkBackend(viona) => {
                viona_backends.insert(id, viona);
            }
            latest::instance_spec::Component::DlpiNetworkBackend(dlpi) => {
                dlpi_backends.insert(id, dlpi);
            }
            device => {
                devices.push((id, device));
            }
        }
    }

    for (device_id, device_spec) in devices {
        match device_spec {
            latest::instance_spec::Component::VirtioDisk(_)
            | latest::instance_spec::Component::NvmeDisk(_) => {
                let device_spec = StorageDevice::try_from(device_spec)
                    .expect("component is known to be a disk");

                let (_, backend_spec) = storage_backends
                    .remove_entry(device_spec.backend_id())
                    .ok_or_else(|| ApiSpecError::StorageBackendNotFound {
                        backend: device_spec.backend_id().to_owned(),
                        device: device_id.clone(),
                    })?;

                builder.add_storage_device(
                    device_id,
                    Disk { device_spec, backend_spec },
                )?;
            }
            latest::instance_spec::Component::VirtioNic(nic) => {
                let (_, backend_spec) = viona_backends
                    .remove_entry(&nic.backend_id)
                    .ok_or_else(|| ApiSpecError::NetworkBackendNotFound {
                        backend: nic.backend_id.clone(),
                        device: device_id.clone(),
                    })?;

                builder.add_network_device(
                    device_id,
                    Nic { device_spec: nic, backend_spec },
                )?;
            }
            latest::instance_spec::Component::SerialPort(port) => {
                builder.add_serial_port(device_id, port.num)?;
            }
            latest::instance_spec::Component::PciPciBridge(bridge) => {
                builder.add_pci_bridge(device_id, bridge)?;
            }
            latest::instance_spec::Component::QemuPvpanic(pvpanic) => {
                builder.add_pvpanic_device(QemuPvpanic {
                    id: device_id,
                    spec: pvpanic,
                })?;
            }
            latest::instance_spec::Component::BootSettings(settings) => {
                // The builder returns an error if its caller tries to add
                // a boot option that isn't in the set of attached disks.
                // Since there may be more disk devices left in the
                // component map, just capture the boot order for now and
                // apply it to the builder later.
                boot_settings = Some((device_id, settings));
            }
            latest::instance_spec::Component::VirtioSocket(vsock) => {
                let vsock_device = crate::spec::VirtioSocket {
                    id: device_id.clone(),
                    spec: vsock,
                };
                builder.add_vsock_device(vsock_device)?;
            }
            #[cfg(not(feature = "failure-injection"))]
            latest::instance_spec::Component::MigrationFailureInjector(_) => {
                return Err(ApiSpecError::FeatureCompiledOut {
                    component: device_id,
                    feature: "failure-injection",
                });
            }
            #[cfg(feature = "failure-injection")]
            latest::instance_spec::Component::MigrationFailureInjector(mig) => {
                builder.add_migration_failure_device(MigrationFailure {
                    id: device_id,
                    spec: mig,
                })?;
            }
            #[cfg(not(feature = "falcon"))]
            latest::instance_spec::Component::SoftNpuPciPort(_)
            | latest::instance_spec::Component::SoftNpuPort(_)
            | latest::instance_spec::Component::SoftNpuP9(_)
            | latest::instance_spec::Component::P9fs(_) => {
                return Err(ApiSpecError::FeatureCompiledOut {
                    component: device_id,
                    feature: "falcon",
                });
            }
            #[cfg(feature = "falcon")]
            latest::instance_spec::Component::SoftNpuPciPort(port) => {
                builder.set_softnpu_pci_port(port)?;
            }
            #[cfg(feature = "falcon")]
            latest::instance_spec::Component::SoftNpuPort(port) => {
                let (_, backend_spec) = dlpi_backends
                    .remove_entry(&port.backend_id)
                    .ok_or_else(|| ApiSpecError::NetworkBackendNotFound {
                        backend: port.backend_id.clone(),
                        device: device_id.clone(),
                    })?;

                let port = SoftNpuPort {
                    link_name: port.link_name,
                    backend_name: port.backend_id,
                    backend_spec,
                };

                builder.add_softnpu_port(device_id, port)?;
            }
            #[cfg(feature = "falcon")]
            latest::instance_spec::Component::SoftNpuP9(p9) => {
                builder.set_softnpu_p9(p9)?;
            }
            #[cfg(feature = "falcon")]
            latest::instance_spec::Component::P9fs(p9fs) => {
                builder.set_p9fs(p9fs)?;
            }
            latest::instance_spec::Component::CrucibleStorageBackend(_)
            | latest::instance_spec::Component::FileStorageBackend(_)
            | latest::instance_spec::Component::BlobStorageBackend(_)
            | latest::instance_spec::Component::VirtioNetworkBackend(_)
            | latest::instance_spec::Component::DlpiNetworkBackend(_) => {
                unreachable!("already filtered out backends")
            }
        }
    }

    // Now that all disks have been attached, try to establish the boot
    // order if one was supplied.
    if let Some(settings) = boot_settings {
        builder.add_boot_order(
            settings.0,
            settings.1.order.into_iter().map(Into::into),
        )?;
    }

    if let Some(backend) = storage_backends.into_keys().next() {
        return Err(ApiSpecError::BackendNotUsed(backend));
    }

    if let Some(backend) = viona_backends.into_keys().next() {
        return Err(ApiSpecError::BackendNotUsed(backend));
    }

    if let Some(backend) = dlpi_backends.into_keys().next() {
        return Err(ApiSpecError::BackendNotUsed(backend));
    }

    Ok(builder)
}

#[cfg(test)]
mod test {
    use super::*;
    use latest::components::board::{
        Board, Chipset, Cpuid, GuestHypervisorInterface, I440Fx,
    };
    use latest::instance_spec::{CpuidVendor, SmbiosType1Input};

    // Regression test: the SMBIOS type 1 input must survive conversion
    // from the API instance spec to the internal spec. The conversion rework in
    // #1178 dropped it, leaving guests with default type 1 contents.
    #[test]
    fn smbios_type1_input_preserved() {
        let api_spec = latest::instance_spec::InstanceSpec {
            board: Board {
                cpus: 4,
                memory_mb: 512,
                chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
                guest_hv_interface: GuestHypervisorInterface::Bhyve,
                // Explicit values keep the builder from querying bhyve for
                // its default guest CPUID set, which needs VMM device access
                // the test runner may lack.
                cpuid: Some(Cpuid {
                    entries: vec![],
                    vendor: CpuidVendor::Amd,
                }),
            },
            components: Default::default(),
            smbios: Some(SmbiosType1Input {
                manufacturer: "a4x2".to_string(),
                product_name: "913-0000019".to_string(),
                serial_number: "2FAKE000".to_string(),
                version: 2,
            }),
        };

        let spec = latest_to_spec_builder(api_spec).unwrap().finish();
        let smbios =
            spec.smbios_type1_input.expect("SMBIOS type 1 input preserved");
        assert_eq!(smbios.manufacturer, "a4x2");
        assert_eq!(smbios.product_name, "913-0000019");
        assert_eq!(smbios.serial_number, "2FAKE000");
        assert_eq!(smbios.version, 2);
    }
}
