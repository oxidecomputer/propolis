// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use cpuid_utils::CpuidIdent;
use propolis_client::types::MigrationFailureInjector;
use propolis_client::{
    types::{
        Board, BootOrderEntry, BootSettings, Chipset, ComponentV0, Cpuid,
        CpuidEntry, CpuidVendor, InstanceMetadata, InstanceSpecV0, NvmeDisk,
        SerialPort, SerialPortNumber, VirtioDisk,
    },
    PciPath, SpecKey,
};
use uuid::Uuid;

use crate::{
    disk::{DeviceName, DiskConfig, DiskSource},
    test_vm::spec::VmSpec,
    Framework,
};

/// The disk interface to use for a given guest disk.
#[derive(Clone, Copy, Debug)]
pub enum DiskInterface {
    Virtio,
    Nvme,
}

#[derive(Clone, Copy, Debug)]
pub enum DiskBackend {
    File,
    Crucible { min_disk_size_gib: u64, block_size: crate::disk::BlockSize },
    InMemory { readonly: bool },
}

#[derive(Clone, Debug)]
struct DiskRequest<'a> {
    name: &'a str,
    interface: DiskInterface,
    backend: DiskBackend,
    source: DiskSource<'a>,
    pci_device_num: u8,
}

pub struct VmConfig<'dr> {
    vm_name: String,
    cpus: u8,
    memory_mib: u64,
    cpuid: Option<Vec<CpuidEntry>>,
    bootrom_artifact: String,
    boot_order: Option<Vec<&'dr str>>,
    migration_failure: Option<MigrationFailureInjector>,
    disks: Vec<DiskRequest<'dr>>,
}

const MIGRATION_FAILURE_DEVICE: &str = "test-migration-failure";

impl<'dr> VmConfig<'dr> {
    pub(crate) fn new(
        vm_name: &str,
        cpus: u8,
        memory_mib: u64,
        bootrom: &str,
        guest_artifact: &'dr str,
    ) -> Self {
        let mut config = Self {
            vm_name: vm_name.to_owned(),
            cpus,
            memory_mib,
            cpuid: None,
            bootrom_artifact: bootrom.to_owned(),
            boot_order: None,
            migration_failure: None,
            disks: Vec::new(),
        };

        config.boot_disk(
            guest_artifact,
            DiskInterface::Nvme,
            DiskBackend::File,
            4,
        );

        config
    }

    pub fn cpus(&mut self, cpus: u8) -> &mut Self {
        self.cpus = cpus;
        self
    }

    pub fn memory_mib(&mut self, mem: u64) -> &mut Self {
        self.memory_mib = mem;
        self
    }

    pub fn bootrom(&mut self, artifact: &str) -> &mut Self {
        artifact.clone_into(&mut self.bootrom_artifact);
        self
    }

    pub fn named(&mut self, name: impl ToString) -> &mut Self {
        self.vm_name = name.to_string();
        self
    }

    pub fn cpuid(&mut self, entries: Vec<CpuidEntry>) -> &mut Self {
        self.cpuid = Some(entries);
        self
    }

    pub fn fail_migration_exports(&mut self, exports: u32) -> &mut Self {
        let injector =
            self.migration_failure.get_or_insert(MigrationFailureInjector {
                fail_exports: 0,
                fail_imports: 0,
            });

        injector.fail_exports = exports;
        self
    }

    pub fn fail_migration_imports(&mut self, imports: u32) -> &mut Self {
        let injector =
            self.migration_failure.get_or_insert(MigrationFailureInjector {
                fail_exports: 0,
                fail_imports: 0,
            });

        injector.fail_imports = imports;
        self
    }

    pub fn boot_order(&mut self, disks: Vec<&'dr str>) -> &mut Self {
        self.boot_order = Some(disks);
        self
    }

    pub fn clear_boot_order(&mut self) -> &mut Self {
        self.boot_order = None;
        self
    }

    /// Add a new disk to the VM config, and add it to the front of the VM's
    /// boot order.
    ///
    /// The added disk will have the name `boot-disk`, and replace the previous
    /// existing `boot-disk`.
    pub fn boot_disk(
        &mut self,
        artifact: &'dr str,
        interface: DiskInterface,
        backend: DiskBackend,
        pci_device_num: u8,
    ) -> &mut Self {
        let boot_order = self.boot_order.get_or_insert(Vec::new());
        if let Some(prev_boot_item) =
            boot_order.iter().position(|d| *d == "boot-disk")
        {
            boot_order.remove(prev_boot_item);
        }

        if let Some(prev_boot_disk) =
            self.disks.iter().position(|d| d.name == "boot-disk")
        {
            self.disks.remove(prev_boot_disk);
        }

        boot_order.insert(0, "boot-disk");

        self.data_disk(
            "boot-disk",
            DiskSource::Artifact(artifact),
            interface,
            backend,
            pci_device_num,
        );

        self
    }

    pub fn data_disk(
        &mut self,
        name: &'dr str,
        source: DiskSource<'dr>,
        interface: DiskInterface,
        backend: DiskBackend,
        pci_device_num: u8,
    ) -> &mut Self {
        self.disks.push(DiskRequest {
            name,
            interface,
            backend,
            source,
            pci_device_num,
        });
        self
    }

    pub(crate) async fn vm_spec(
        &self,
        framework: &Framework,
    ) -> anyhow::Result<VmSpec> {
        // Exhaustively destructure to break the build if a new field is added
        // but not considered here.
        let VmConfig {
            vm_name,
            cpus,
            memory_mib,
            cpuid,
            bootrom_artifact,
            boot_order,
            migration_failure,
            disks,
        } = &self;

        let bootrom = framework
            .artifact_store
            .get_bootrom(bootrom_artifact)
            .await
            .context("looking up bootrom artifact")?;

        // The first disk in the boot list might not be the disk a test
        // *actually* expects to boot.
        //
        // If there are multiple bootable disks in the boot order, we'll assume
        // they're all the same guest OS kind. So look for `boot-disk` - if
        // there isn't a disk named `boot-disk` then fall back to hoping the
        // first disk in the boot order is a bootable disk, and if *that* isn't
        // a bootable disk, maybe the first disk is.
        //
        // TODO: theoretically we might want to accept configuration of a
        // specific guest OS adapter and avoid the guessing games. So far the
        // above supports existing tests and makes them "Just Work", but a more
        // complicated test may want more control here.
        let boot_disk = self
            .disks
            .iter()
            .find(|d| d.name == "boot-disk")
            .or_else(|| {
                if let Some(boot_order) = boot_order.as_ref() {
                    boot_order.first().and_then(|name| {
                        self.disks.iter().find(|d| &d.name == name)
                    })
                } else {
                    None
                }
            })
            .or_else(|| disks.first())
            .expect("VM config includes at least one disk");

        // XXX: assuming all bootable images are equivalent to the first, or at
        // least the same guest OS kind.
        let DiskSource::Artifact(boot_artifact) = boot_disk.source else {
            unreachable!("boot disks always use artifacts as sources");
        };

        let (_, guest_os_kind) = framework
            .artifact_store
            .get_guest_os_image(boot_artifact)
            .await
            .context("getting guest OS kind for boot disk")?;

        let mut disk_handles = Vec::new();
        for disk in disks.iter() {
            disk_handles.push(
                make_disk(disk.name.to_owned(), framework, disk)
                    .await
                    .context("creating disk")?,
            );
        }

        let host_leaf_0 = cpuid_utils::host_query(CpuidIdent::leaf(0));
        let host_vendor = cpuid_utils::CpuidVendor::try_from(host_leaf_0)
            .map_err(|_| {
                anyhow::anyhow!(
                    "unknown host CPU vendor (leaf 0: {host_leaf_0:?})"
                )
            })?;

        let mut spec = InstanceSpecV0 {
            board: Board {
                cpus: *cpus,
                memory_mb: *memory_mib,
                chipset: Chipset::default(),
                cpuid: cpuid.as_ref().map(|entries| Cpuid {
                    entries: entries.clone(),
                    vendor: match host_vendor {
                        cpuid_utils::CpuidVendor::Amd => CpuidVendor::Amd,
                        cpuid_utils::CpuidVendor::Intel => CpuidVendor::Intel,
                    },
                }),
            },
            components: Default::default(),
        };

        // Iterate over the collection of disks and handles and add spec
        // elements for all of them. This assumes the disk handles were created
        // in the correct order: boot disk first, then in the data disks'
        // iteration order.
        let all_disks = disks.iter().zip(disk_handles.iter());
        for (req, hdl) in all_disks {
            let pci_path = PciPath::new(0, req.pci_device_num, 0).unwrap();
            let backend_spec = hdl.backend_spec();
            let device_name = hdl.device_name().clone();
            let backend_id = SpecKey::Name(
                device_name.clone().into_backend_name().into_string(),
            );
            let device_spec = match req.interface {
                DiskInterface::Virtio => ComponentV0::VirtioDisk(VirtioDisk {
                    backend_id: backend_id.clone(),
                    pci_path,
                }),
                DiskInterface::Nvme => ComponentV0::NvmeDisk(NvmeDisk {
                    backend_id: backend_id.clone(),
                    pci_path,
                }),
            };

            let _old =
                spec.components.insert(device_name.into_string(), device_spec);
            assert!(_old.is_none());
            let _old =
                spec.components.insert(backend_id.to_string(), backend_spec);
            assert!(_old.is_none());
        }

        let _old = spec.components.insert(
            "com1".to_string(),
            ComponentV0::SerialPort(SerialPort { num: SerialPortNumber::Com1 }),
        );
        assert!(_old.is_none());

        if let Some(boot_order) = boot_order.as_ref() {
            let _old = spec.components.insert(
                "boot-settings".to_string(),
                ComponentV0::BootSettings(BootSettings {
                    order: boot_order
                        .iter()
                        .map(|item| BootOrderEntry {
                            component_id: SpecKey::from_str(item).unwrap(),
                        })
                        .collect(),
                }),
            );
            assert!(_old.is_none());
        }

        if let Some(migration_failure) = migration_failure {
            let _old = spec.components.insert(
                MIGRATION_FAILURE_DEVICE.to_owned(),
                ComponentV0::MigrationFailureInjector(
                    migration_failure.clone(),
                ),
            );
            assert!(_old.is_none());
        }

        // Generate random identifiers for this instance's timeseries metadata.
        let sled_id = Uuid::new_v4();
        let metadata = InstanceMetadata {
            project_id: Uuid::new_v4(),
            silo_id: Uuid::new_v4(),
            sled_id,
            sled_model: "pheidippes".into(),
            sled_revision: 1,
            sled_serial: sled_id.to_string(),
        };

        Ok(VmSpec {
            vm_name: vm_name.clone(),
            instance_spec: spec,
            disk_handles,
            guest_os_kind,
            bootrom_path: bootrom,
            metadata,
        })
    }
}

async fn make_disk<'req>(
    device_name: String,
    framework: &Framework,
    req: &DiskRequest<'req>,
) -> anyhow::Result<Arc<dyn DiskConfig>> {
    let device_name = DeviceName::new(device_name);

    Ok(match req.backend {
        DiskBackend::File => framework
            .disk_factory
            .create_file_backed_disk(device_name, &req.source)
            .await
            .with_context(|| {
                format!("creating new file-backed disk from {:?}", req.source,)
            })? as Arc<dyn DiskConfig>,
        DiskBackend::Crucible { min_disk_size_gib, block_size } => framework
            .disk_factory
            .create_crucible_disk(
                device_name,
                &req.source,
                min_disk_size_gib,
                block_size,
            )
            .await
            .with_context(|| {
                format!(
                    "creating new Crucible-backed disk from {:?}",
                    req.source,
                )
            })?
            as Arc<dyn DiskConfig>,
        DiskBackend::InMemory { readonly } => framework
            .disk_factory
            .create_in_memory_disk(device_name, &req.source, readonly)
            .await
            .with_context(|| {
                format!("creating new in-memory disk from {:?}", req.source)
            })?
            as Arc<dyn DiskConfig>,
    })
}
