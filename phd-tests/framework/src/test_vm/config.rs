// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context;
use propolis_client::{
    instance_spec::SpecBuilderV0,
    types::{
        InstanceMetadata, NvmeDisk, PciPath, SerialPortNumber, StorageDeviceV0,
        VirtioDisk,
    },
};
use uuid::Uuid;

use crate::{
    disk::{DiskConfig, DiskSource},
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
    bootrom_artifact: String,
    boot_order: Option<Vec<&'dr str>>,
    disks: Vec<DiskRequest<'dr>>,
    devices: BTreeMap<String, propolis_server_config::Device>,
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
            bootrom_artifact: bootrom.to_owned(),
            boot_order: None,
            disks: Vec::new(),
            devices: BTreeMap::new(),
        };

        config.data_disk(
            "boot-disk",
            DiskSource::Artifact(guest_artifact),
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

    pub fn fail_migration_exports(&mut self, exports: u32) -> &mut Self {
        self.devices
            .entry(MIGRATION_FAILURE_DEVICE.to_owned())
            .or_insert_with(default_migration_failure_device)
            .options
            .insert("fail_exports".to_string(), exports.into());
        self
    }

    pub fn fail_migration_imports(&mut self, imports: u32) -> &mut Self {
        self.devices
            .entry(MIGRATION_FAILURE_DEVICE.to_owned())
            .or_insert_with(default_migration_failure_device)
            .options
            .insert("fail_imports".to_string(), imports.into());
        self
    }

    pub fn boot_order(&mut self, disks: Vec<&'dr str>) -> &mut Self {
        self.boot_order = Some(disks);
        self
    }

    pub fn boot_disk(
        &mut self,
        artifact: &'dr str,
        interface: DiskInterface,
        backend: DiskBackend,
        pci_device_num: u8,
    ) -> &mut Self {
        // If you're calling `boot_disk` directly, you're probably not specifying an explicit boot
        // order. the _implicit_ boot order seems to be the order disks are created in, so preserve
        // that implied behavior by having a boot disk inserted up front.
        //
        // XXX: i thought that the implicit order would be *pci device order*, not disk creation
        // order?
        // XXX: figure out what to do if there is both an explicit boot order and a call to
        // `.boot_disk`.

        self.disks[0] = DiskRequest {
            name: "boot-disk",
            interface,
            backend,
            source: DiskSource::Artifact(artifact),
            pci_device_num,
        };

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
        // Figure out where the bootrom is and generate the serialized contents
        // of a Propolis server config TOML that points to it.
        let bootrom = framework
            .artifact_store
            .get_bootrom(&self.bootrom_artifact)
            .await
            .context("looking up bootrom artifact")?;

        let config_toml_contents =
            toml::ser::to_string(&propolis_server_config::Config {
                bootrom: bootrom.clone().into(),
                devices: self.devices.clone(),
                ..Default::default()
            })
            .context("serializing Propolis server config")?;

        let boot_disk = &self.disks[0];

        // TODO: adapt this to check all listed boot devices
        let DiskSource::Artifact(boot_artifact) = boot_disk.source else {
            unreachable!("boot disks always use artifacts as sources");
        };

        let (_, guest_os_kind) = framework
            .artifact_store
            .get_guest_os_image(boot_artifact)
            .await
            .context("getting guest OS kind for boot disk")?;

        let mut disk_handles = Vec::new();
        for disk in self.disks.iter() {
            disk_handles.push(
                // format!("data-disk-{}", idx)
                make_disk(disk.name.to_owned(), framework, disk)
                    .await
                    .context("creating disk")?,
            );
        }

        let mut spec_builder =
            SpecBuilderV0::new(self.cpus, self.memory_mib, false);

        // Iterate over the collection of disks and handles and add spec
        // elements for all of them. This assumes the disk handles were created
        // in the correct order: boot disk first, then in the data disks'
        // iteration order.
        let all_disks = self.disks.iter().zip(disk_handles.iter());
        for (idx, (req, hdl)) in all_disks.enumerate() {
            let device_name = format!("disk-device{}", idx);
            let pci_path = PciPath::new(0, req.pci_device_num, 0).unwrap();
            let (backend_name, backend_spec) = hdl.backend_spec();
            let device_spec = match req.interface {
                DiskInterface::Virtio => {
                    StorageDeviceV0::VirtioDisk(VirtioDisk {
                        backend_name: backend_name.clone(),
                        pci_path,
                    })
                }
                DiskInterface::Nvme => StorageDeviceV0::NvmeDisk(NvmeDisk {
                    backend_name: backend_name.clone(),
                    pci_path,
                }),
            };

            spec_builder
                .add_storage_device(
                    device_name,
                    device_spec,
                    backend_name,
                    backend_spec,
                )
                .context("adding storage device to spec")?;
        }

        spec_builder
            .add_serial_port(SerialPortNumber::Com1)
            .context("adding serial port to spec")?;

        if let Some(boot_order) = self.boot_order.as_ref() {
            spec_builder
                .set_boot_order(
                    boot_order.iter().map(|x| x.to_string()).collect(),
                )
                .context("adding boot order to spec")?;
        }

        let instance_spec = spec_builder.finish();

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
            vm_name: self.vm_name.clone(),
            instance_spec,
            disk_handles,
            guest_os_kind,
            config_toml_contents,
            metadata,
        })
    }
}

async fn make_disk<'req>(
    name: String,
    framework: &Framework,
    req: &DiskRequest<'req>,
) -> anyhow::Result<Arc<dyn DiskConfig>> {
    Ok(match req.backend {
        DiskBackend::File => framework
            .disk_factory
            .create_file_backed_disk(name, &req.source)
            .await
            .with_context(|| {
                format!("creating new file-backed disk from {:?}", req.source,)
            })? as Arc<dyn DiskConfig>,
        DiskBackend::Crucible { min_disk_size_gib, block_size } => framework
            .disk_factory
            .create_crucible_disk(
                name,
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
            .create_in_memory_disk(name, &req.source, readonly)
            .await
            .with_context(|| {
                format!("creating new in-memory disk from {:?}", req.source)
            })?
            as Arc<dyn DiskConfig>,
    })
}

fn default_migration_failure_device() -> propolis_server_config::Device {
    propolis_server_config::Device {
        driver: MIGRATION_FAILURE_DEVICE.to_owned(),
        options: Default::default(),
    }
}
