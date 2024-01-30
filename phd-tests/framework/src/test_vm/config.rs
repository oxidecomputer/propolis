// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context;
use propolis_client::{
    instance_spec::SpecBuilderV0,
    types::{NvmeDisk, PciPath, SerialPortNumber, StorageDeviceV0, VirtioDisk},
};

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
}

#[derive(Clone, Debug)]
struct DiskRequest {
    interface: DiskInterface,
    backend: DiskBackend,
    source_artifact: String,
    pci_device_num: u8,
}

pub struct VmConfig {
    vm_name: String,
    cpus: u8,
    memory_mib: u64,
    bootrom_artifact: String,
    boot_disk: DiskRequest,
    data_disks: Vec<DiskRequest>,
    devices: BTreeMap<String, propolis_server_config::Device>,
}

const MIGRATION_FAILURE_DEVICE: &str = "test-migration-failure";

impl VmConfig {
    pub(crate) fn new(
        vm_name: &str,
        cpus: u8,
        memory_mib: u64,
        bootrom: &str,
        guest_artifact: &str,
    ) -> Self {
        let boot_disk = DiskRequest {
            interface: DiskInterface::Nvme,
            backend: DiskBackend::File,
            source_artifact: guest_artifact.to_owned(),
            pci_device_num: 4,
        };

        Self {
            vm_name: vm_name.to_owned(),
            cpus,
            memory_mib,
            bootrom_artifact: bootrom.to_owned(),
            boot_disk,
            data_disks: Vec::new(),
            devices: BTreeMap::new(),
        }
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
        self.bootrom_artifact = artifact.to_owned();
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

    pub fn boot_disk(
        &mut self,
        artifact: &str,
        interface: DiskInterface,
        backend: DiskBackend,
        pci_device_num: u8,
    ) -> &mut Self {
        self.boot_disk = DiskRequest {
            interface,
            backend,
            source_artifact: artifact.to_owned(),
            pci_device_num,
        };
        self
    }

    pub fn data_disk(
        &mut self,
        artifact: &str,
        interface: DiskInterface,
        backend: DiskBackend,
        pci_device_num: u8,
    ) -> &mut Self {
        self.data_disks.push(DiskRequest {
            interface,
            backend,
            source_artifact: artifact.to_owned(),
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

        let (_, guest_os_kind) = framework
            .artifact_store
            .get_guest_os_image(&self.boot_disk.source_artifact)
            .await
            .context("getting guest OS kind for boot disk")?;

        let mut disk_handles = Vec::new();
        disk_handles.push(
            make_disk("boot-disk".to_owned(), framework, &self.boot_disk)
                .await
                .context("creating boot disk")?,
        );
        for (idx, data_disk) in self.data_disks.iter().enumerate() {
            disk_handles.push(
                make_disk(format!("data-disk-{}", idx), framework, data_disk)
                    .await
                    .context("creating data disk")?,
            );
        }

        let mut spec_builder =
            SpecBuilderV0::new(self.cpus, self.memory_mib, false);

        // Iterate over the collection of disks and handles and add spec
        // elements for all of them. This assumes the disk handles were created
        // in the correct order: boot disk first, then in the data disks'
        // iteration order.
        let all_disks = [&self.boot_disk]
            .into_iter()
            .chain(self.data_disks.iter())
            .zip(disk_handles.iter());
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

        let instance_spec = spec_builder.finish();

        Ok(VmSpec {
            vm_name: self.vm_name.clone(),
            instance_spec,
            disk_handles,
            guest_os_kind,
            config_toml_contents,
        })
    }
}

async fn make_disk(
    name: String,
    framework: &Framework,
    req: &DiskRequest,
) -> anyhow::Result<Arc<dyn DiskConfig>> {
    let source = DiskSource::Artifact(&req.source_artifact);
    Ok(match req.backend {
        DiskBackend::File => framework
            .disk_factory
            .create_file_backed_disk(name, source)
            .await
            .with_context(|| {
                format!(
                    "creating new file-backed disk from '{}'",
                    &req.source_artifact
                )
            })? as Arc<dyn DiskConfig>,
        DiskBackend::Crucible { min_disk_size_gib, block_size } => framework
            .disk_factory
            .create_crucible_disk(name, source, min_disk_size_gib, block_size)
            .await
            .with_context(|| {
                format!(
                    "creating new Crucible-backed disk from \
                                    '{}'",
                    &req.source_artifact
                )
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
