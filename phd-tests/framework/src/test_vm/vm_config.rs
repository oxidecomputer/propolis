//! Structures that express how a VM should be configured.

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::Result;
use propolis_server_config as config;

#[derive(Debug)]
enum DiskInterface {
    Virtio,
    Nvme,
}

#[derive(Debug)]
struct Disk {
    image_path: String,
    pci_device_num: u8,
    interface: DiskInterface,
}

#[derive(Debug)]
struct Nic {
    vnic_device_name: String,
    pci_device_num: u8,
}

/// An abstract description of a VM's configuration: its CPUs, memory, and
/// devices.
#[derive(Debug, Default)]
pub struct VmConfig {
    cpus: u8,
    memory_mib: u64,
    bootrom_path: PathBuf,
    disks: Vec<Disk>,
    nics: Vec<Nic>,
}

impl VmConfig {
    /// Returns the number of CPUs in this config.
    pub fn cpus(&self) -> u8 {
        self.cpus
    }

    /// Returns the amount of memory in this config.
    pub fn memory_mib(&self) -> u64 {
        self.memory_mib
    }

    // Writes those portions of the config that are specified to Propolis
    // servers via the config TOML into a file containing said TOML.
    pub fn write_config_toml(
        &self,
        toml_path: &impl AsRef<Path>,
    ) -> Result<()> {
        // TODO: Change this to use instance specs when Propolis has an API that
        // accepts those.
        let bootrom: PathBuf = self.bootrom_path.clone().into();
        let chipset = config::Chipset { options: BTreeMap::default() };

        let mut device_map: BTreeMap<String, config::Device> = BTreeMap::new();
        let mut backend_map: BTreeMap<String, config::BlockDevice> =
            BTreeMap::new();

        let mut disk_idx = 0;
        for disk in &self.disks {
            let backend_name = format!("block{}", disk_idx);
            let backend = config::BlockDevice {
                bdtype: "file".to_string(),
                options: BTreeMap::from([
                    ("path".to_string(), disk.image_path.clone().into()),
                    ("readonly".to_string(), false.into()),
                ]),
            };

            let device_name = match disk.interface {
                DiskInterface::Virtio => format!("vioblk{}", disk_idx),
                DiskInterface::Nvme => format!("nvme{}", disk_idx),
            };
            let device = config::Device {
                driver: match disk.interface {
                    DiskInterface::Virtio => "pci-virtio-block".to_string(),
                    DiskInterface::Nvme => "pci-nvme".to_string(),
                },
                options: BTreeMap::from([
                    ("block_dev".to_string(), backend_name.clone().into()),
                    (
                        "pci-path".to_string(),
                        format!("0.{}.0", disk.pci_device_num).into(),
                    ),
                ]),
            };

            device_map.insert(device_name, device);
            backend_map.insert(backend_name, backend);
            disk_idx += 1;
        }

        let mut vnic_idx = 0;
        for vnic in &self.nics {
            let device_name = format!("viona{}", vnic_idx);
            let device = config::Device {
                driver: "pci-virtio-viona".to_string(),
                options: BTreeMap::from([
                    ("vnic".to_string(), vnic.vnic_device_name.clone().into()),
                    (
                        "pci-path".to_string(),
                        format!("0.{}.0", vnic.pci_device_num).into(),
                    ),
                ]),
            };

            device_map.insert(device_name, device);
            vnic_idx += 1;
        }

        let config = config::Config::new(
            bootrom,
            chipset,
            device_map,
            backend_map,
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

/// A builder for [`VmConfig`] structures.
pub struct VmConfigBuilder {
    config: VmConfig,
}

impl VmConfigBuilder {
    /// Creates a new, empty builder.
    pub(crate) fn new() -> Self {
        Self { config: Default::default() }
    }

    /// Sets the number of CPUs in the config.
    pub fn set_cpus(mut self, cpus: u8) -> Self {
        self.config.cpus = cpus;
        self
    }

    /// Sets the amount of memory in the config.
    pub fn set_memory_mib(mut self, mem: u64) -> Self {
        self.config.memory_mib = mem;
        self
    }

    /// Sets the config's bootrom path.
    pub fn set_bootrom_path(mut self, path: PathBuf) -> Self {
        self.config.bootrom_path = path;
        self
    }

    /// Adds a disk descriptor for a virtio-block disk device backed by the file
    /// at `image_path`, which will manifest to the guest at PCI BDF
    /// 0/`pci_slot`/0.
    pub fn add_virtio_block_disk(
        mut self,
        image_path: &str,
        pci_slot: u8,
    ) -> Self {
        self.config.disks.push(Disk {
            image_path: image_path.to_string(),
            pci_device_num: pci_slot,
            interface: DiskInterface::Virtio,
        });
        self
    }

    /// Adds a disk descriptor for an NVMe device backed by the file at
    /// `image_path`, which will manifest to the guest at PCI BDF
    /// 0/`pci_slot`/0.
    pub fn add_nvme_disk(mut self, image_path: &str, pci_slot: u8) -> Self {
        self.config.disks.push(Disk {
            image_path: image_path.to_string(),
            pci_device_num: pci_slot,
            interface: DiskInterface::Nvme,
        });
        self
    }

    /// Adds a NIC descriptor for a virtio network device backed by the vNIC
    /// with the supplied name, which will manifest to the guest at PCI BDF
    /// 0/`pci_slot`/0.
    pub fn add_vnic(mut self, host_vnic_name: &str, pci_slot: u8) -> Self {
        self.config.nics.push(Nic {
            vnic_device_name: host_vnic_name.to_string(),
            pci_device_num: pci_slot,
        });
        self
    }

    /// Creates the configuration described by this builder.
    pub fn finish(self) -> VmConfig {
        self.config
    }
}
