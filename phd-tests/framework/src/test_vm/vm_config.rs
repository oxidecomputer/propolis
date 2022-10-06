//! Structures that express how a VM should be configured.

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use propolis_client::instance_spec::{InstanceSpec, StorageDeviceKind};
use propolis_server_config as config;
use propolis_types::PciPath;
use thiserror::Error;

use crate::{disk, guest_os::GuestOsKind};

/// Errors raised when configuring a VM or realizing a requested configuration.
#[derive(Debug, Error)]
pub enum VmConfigError {
    #[error("No boot disk specified")]
    NoBootDisk,

    #[error("Boot disk does not have an associated guest OS")]
    BootDiskNotGuestImage,

    #[error("Could not find artifact {0} when populating disks")]
    ArtifactNotFound(String),
}

/// The disk interface to use for a given guest disk.
#[derive(Clone, Copy, Debug)]
pub enum DiskInterface {
    Virtio,
    Nvme,
}

impl From<DiskInterface> for StorageDeviceKind {
    fn from(interface: DiskInterface) -> Self {
        match interface {
            DiskInterface::Virtio => StorageDeviceKind::Virtio,
            DiskInterface::Nvme => StorageDeviceKind::Nvme,
        }
    }
}

/// Parameters used to initialize a guest disk.
#[derive(Debug)]
struct DiskRequest {
    /// A reference to the resources needed to create this disk's backend. VMs
    /// created from this configuration also get a reference to these resources.
    disk: Arc<disk::GuestDisk>,

    /// The PCI device number to assign to this disk. The disk's BDF will be
    /// 0/this value/0.
    pci_device_num: u8,

    /// The PCI device interface to present to the guest.
    interface: DiskInterface,
}

/// An abstract description of a test VM's configuration and any objects needed
/// to launch the VM with that configuration.
#[derive(Default, Debug)]
pub struct ConfigRequest {
    cpus: u8,
    memory_mib: u64,
    bootrom_path: PathBuf,
    boot_disk: Option<DiskRequest>,
    data_disks: Vec<DiskRequest>,
}

impl ConfigRequest {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub fn set_cpus(mut self, cpus: u8) -> Self {
        self.cpus = cpus;
        self
    }

    pub fn set_memory_mib(mut self, mem: u64) -> Self {
        self.memory_mib = mem;
        self
    }

    pub fn set_bootrom_path(mut self, path: PathBuf) -> Self {
        self.bootrom_path = path;
        self
    }

    pub fn set_boot_disk(
        mut self,
        disk: Arc<disk::GuestDisk>,
        pci_device_num: u8,
        interface: DiskInterface,
    ) -> Self {
        self.boot_disk = Some(DiskRequest { disk, pci_device_num, interface });
        self
    }

    fn all_disks(&self) -> impl Iterator<Item = &DiskRequest> {
        self.boot_disk.iter().chain(self.data_disks.iter())
    }

    pub fn finish(
        self,
        object_dir: &impl AsRef<Path>,
        toml_filename: &impl AsRef<Path>,
    ) -> anyhow::Result<VmConfig> {
        let guest_os_kind = if let Some(disk_request) = &self.boot_disk {
            disk_request
                .disk
                .guest_os()
                .ok_or_else(|| VmConfigError::BootDiskNotGuestImage)?
        } else {
            return Err(VmConfigError::NoBootDisk.into());
        };

        let mut spec_builder = propolis_client::instance_spec::SpecBuilder::new(
            self.cpus,
            self.memory_mib,
            false,
        );

        let mut disk_handles = Vec::new();
        for (disk_idx, disk_req) in self.all_disks().enumerate() {
            let device_name = format!("disk-device{}", disk_idx);
            let backend_name = format!("storage-backend{}", disk_idx);
            let device_spec = propolis_client::instance_spec::StorageDevice {
                kind: disk_req.interface.into(),
                backend_name: backend_name.clone(),
                pci_path: PciPath::new(0, disk_req.pci_device_num, 0)?,
            };
            let backend_spec = disk_req.disk.backend_spec();

            spec_builder.add_storage_device(
                device_name,
                device_spec,
                backend_name,
                backend_spec,
            )?;

            disk_handles.push(disk_req.disk.clone());
        }

        spec_builder.add_serial_port(
            propolis_client::instance_spec::SerialPortNumber::Com1,
        )?;

        let instance_spec = spec_builder.finish();

        // Propolis selects its bootrom via the config TOML, so write a shell
        // TOML that specifies this. All other VM configuration comes from the
        // instance spec.
        let mut server_toml_path = object_dir.as_ref().to_owned();
        server_toml_path.push(toml_filename);
        self.write_config_toml(&server_toml_path)?;

        Ok(VmConfig {
            instance_spec,
            _disk_handles: disk_handles,
            guest_os_kind,
            server_toml_path,
        })
    }

    fn write_config_toml(&self, toml_path: &Path) -> anyhow::Result<()> {
        let config = config::Config::new(
            self.bootrom_path.clone(),
            config::Chipset { options: BTreeMap::default() },
            BTreeMap::new(),
            BTreeMap::new(),
            Vec::new(),
        );

        let serialized = toml::ser::to_string(&config).unwrap();
        let mut cfg_file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(toml_path)?;

        cfg_file.write_all(serialized.as_bytes())?;

        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct VmConfig {
    instance_spec: InstanceSpec,
    _disk_handles: Vec<Arc<disk::GuestDisk>>,
    guest_os_kind: GuestOsKind,
    server_toml_path: PathBuf,
}

impl VmConfig {
    pub fn instance_spec(&self) -> &InstanceSpec {
        &self.instance_spec
    }

    pub fn guest_os_kind(&self) -> GuestOsKind {
        self.guest_os_kind
    }

    pub fn server_toml_path(&self) -> &PathBuf {
        &self.server_toml_path
    }
}
