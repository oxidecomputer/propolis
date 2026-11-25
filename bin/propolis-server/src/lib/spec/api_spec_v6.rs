// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions between [`propolis_api_types_versions::v6`] instance specs and
//! the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance::ReplacementComponent;
use propolis_api_types::instance_spec::SpecKey;
use propolis_api_types_versions::{v3, v6, v9};

use super::{
    builder::SpecBuilder, Disk, LegacyApiSpecError, Spec, StorageDevice,
};
use crate::migrate::MigrateError;
use crate::spec::api_spec_v9;

impl TryFrom<Spec> for v6::instance_spec::InstanceSpec {
    type Error = LegacyApiSpecError;

    fn try_from(mut val: Spec) -> Result<Self, Self::Error> {
        // v6 adds a new field on NvmeDisk. Such disks probably can't be
        // converted to v3 components and would cause a conversion from
        // Spec->v3::instance_spec::InstanceSpec to fail. So, extract those
        // disks and convert the rest of the Spec to a
        // v3::instance_spec::InstanceSpec. If this fails, we wouldn't have been
        // able to get a v6 spec anyway. If it succeeds, we can add the disks
        // back in here.
        //
        // TODO: could be extract_if once we're on a Rust >= 1.91.0.
        let mut nvme_disks = Vec::new();
        let v6_only_disk =
            |disk: &Disk| matches!(disk.device_spec, StorageDevice::Nvme(_));
        for (key, disk) in val.disks.iter() {
            if v6_only_disk(disk) {
                nvme_disks.push((key.clone(), disk.clone()));
            }
        }
        val.disks.retain(|_, disk| !v6_only_disk(disk));

        let v3_spec: v3::instance_spec::InstanceSpec = val.try_into()?;

        let mut spec: v6::instance_spec::InstanceSpec = v3_spec.into();

        // Inserts a component entry into the supplied map, asserting first that
        // the supplied key is not present in that map.
        //
        // This assertion is valid because internal instance specs should assign
        // a unique name to each component they describe. The spec builder
        // upholds this invariant at spec creation time.
        #[track_caller]
        fn insert_component(
            spec: &mut v6::instance_spec::InstanceSpec,
            key: SpecKey,
            val: v6::instance_spec::Component,
        ) {
            assert!(
                !spec.components.contains_key(&key),
                "component name {} already exists in output spec",
                key
            );
            spec.components.insert(key, val);
        }

        for (disk_id, disk) in nvme_disks {
            let backend_id = disk.device_spec.backend_id().to_owned();
            let device_component: v6::instance_spec::Component =
                disk.device_spec.into();
            let backend_component: v6::instance_spec::Component =
                disk.backend_spec.into();
            insert_component(&mut spec, disk_id, device_component);
            insert_component(&mut spec, backend_id, backend_component);
        }

        Ok(spec)
    }
}

/// Parses a v6 instance spec into a [`SpecBuilder`], validating component
/// names, PCI paths, and backend references along the way. Callers can add
/// additional (non-v6) components to the builder before calling `finish()`.
pub(crate) fn v6_to_spec_builder(
    value: v6::instance_spec::InstanceSpec,
) -> Result<SpecBuilder, LegacyApiSpecError> {
    let v9_spec: v9::instance_spec::InstanceSpec = value.into();
    Ok(api_spec_v9::v9_to_spec_builder(v9_spec)?)
}

// pub(crate) as the next version (v9) still uses
// `v6::instance_spec::Component`, so this conversion is applicable in that
// module as well.
pub(crate) fn amend_component(
    id: &SpecKey,
    to_amend: &mut v6::instance_spec::Component,
    replacement: &ReplacementComponent,
) -> Result<(), MigrateError> {
    match replacement {
        #[cfg(not(feature = "failure-injection"))]
        ReplacementComponent::MigrationFailureInjector(_) => {
            return Err(MigrateError::InstanceSpecsIncompatible(format!(
                "replacing migration failure injector {id} is \
                    impossible because the feature is compiled out"
            )));
        }

        #[cfg(feature = "failure-injection")]
        ReplacementComponent::MigrationFailureInjector(comp) => {
            let v6::instance_spec::Component::MigrationFailureInjector(src) =
                to_amend
            else {
                return Err(MigrateError::wrong_type(
                    id,
                    "migration failure injector",
                ));
            };

            *src = comp.clone();
        }
        ReplacementComponent::CrucibleStorageBackend(comp) => {
            let v6::instance_spec::Component::CrucibleStorageBackend(src) =
                to_amend
            else {
                return Err(MigrateError::wrong_type(id, "crucible backend"));
            };

            *src = comp.clone();
        }
        ReplacementComponent::VirtioNetworkBackend(comp) => {
            let v6::instance_spec::Component::VirtioNetworkBackend(src) =
                to_amend
            else {
                return Err(MigrateError::wrong_type(id, "viona backend"));
            };

            *src = comp.clone();
        }
    }

    Ok(())
}

pub(crate) fn amend(
    spec: &mut v6::instance_spec::InstanceSpec,
    replacements: &BTreeMap<SpecKey, ReplacementComponent>,
) -> Result<(), MigrateError> {
    for (id, replacement) in replacements {
        let Some(to_amend) = spec.components.get_mut(id) else {
            return Err(MigrateError::InstanceSpecsIncompatible(format!(
                "replacement component {id} not in source spec",
            )));
        };

        amend_component(id, to_amend, replacement)?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use propolis_api_types_versions::latest::components::board::{
        Board, Chipset, CpuTopology, Cpuid, GuestHypervisorInterface, I440Fx,
    };
    use propolis_api_types_versions::latest::{
        self,
        components::{backends::BlobStorageBackend, devices::NvmeDisk},
        instance_spec::{self, CpuidVendor, SmbiosType1Input, SpecKey},
    };
    use propolis_api_types_versions::{v3, v6, v9};

    #[test]
    fn nvme_vwc() {
        let mut api_spec = latest::instance_spec::InstanceSpec {
            board: Board {
                cpus: 4,
                memory_mb: 512,
                chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
                guest_hv_interface: GuestHypervisorInterface::Bhyve,
                // Providing *any* CPUID settings keeps Propolis from querying
                // bhyve for defaults to use instead, which requires .. byhve
                // *and* VMM access. Provide a (useless) empty CPUID set so this
                // test can run on non-illumos test systems.
                cpuid: Some(Cpuid {
                    entries: vec![],
                    vendor: CpuidVendor::Amd,
                }),
                cpu_topology: CpuTopology::legacy_single_thread_sockets(4),
            },
            components: Default::default(),
            smbios: Some(SmbiosType1Input {
                manufacturer: "a4x2".to_string(),
                product_name: "913-0000019".to_string(),
                serial_number: "2FAKE000".to_string(),
                version: 2,
            }),
        };

        let nvme_backend_id: SpecKey =
            SpecKey::Name("nvme-backend-id".to_string());
        let nvme_device_id: SpecKey =
            SpecKey::Name("nvme-device-id".to_string());
        let test_backend = BlobStorageBackend {
            base64: "AAAAAAAA".to_string(),
            readonly: true,
        };
        let test_disk = NvmeDisk {
            backend_id: nvme_backend_id.clone(),
            pci_path: instance_spec::PciPath::new(0, 4, 0).unwrap(),
            serial_number: [0u8; 20],
            has_write_cache: false,
        };

        let disk_backend_comp =
            instance_spec::Component::BlobStorageBackend(test_backend);
        let disk_comp = instance_spec::Component::NvmeDisk(test_disk);

        api_spec
            .components
            .insert(nvme_backend_id.clone(), disk_backend_comp.clone());
        api_spec.components.insert(nvme_device_id.clone(), disk_comp.clone());

        let spec =
            crate::spec::api_spec_latest::latest_to_spec_builder(api_spec)
                .unwrap()
                .finish();

        let v9_spec = v9::instance_spec::InstanceSpec::from(spec.clone());
        let v6_comp: v6::instance_spec::Component = disk_comp.clone();
        assert_eq!(v9_spec.components.get(&nvme_device_id), Some(&v6_comp));

        let v6_spec = v6::instance_spec::InstanceSpec::try_from(spec.clone())
            .expect("can convert v6-conformant spec to v6");
        let v6_comp: v6::instance_spec::Component = disk_comp.clone();
        assert_eq!(v6_spec.components.get(&nvme_device_id), Some(&v6_comp));

        let v3_res = v3::instance_spec::InstanceSpec::try_from(spec.clone());
        assert!(v3_res.is_err());
    }
}
