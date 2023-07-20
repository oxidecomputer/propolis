// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::convert::TryInto;
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crucible_client_types::VolumeConstructionRequest;
use oximeter::types::ProducerRegistry;
use propolis::block;
use propolis::chardev::{self, BlockingSource, Source};
use propolis::common::PAGE_SIZE;
use propolis::hw::chipset::i440fx;
use propolis::hw::chipset::i440fx::I440Fx;
use propolis::hw::chipset::Chipset;
use propolis::hw::ibmpc;
use propolis::hw::pci;
use propolis::hw::ps2::ctrl::PS2Ctrl;
use propolis::hw::qemu::{debug::QemuDebugPort, fwcfg, ramfb};
use propolis::hw::uart::LpcUart;
use propolis::hw::{nvme, virtio};
use propolis::instance::Instance;
use propolis::inventory::{self, EntityID, Inventory};
use propolis::vmm::{self, Builder, Machine};
use propolis_client::instance_spec::{self, v0::InstanceSpecV0};
use slog::info;

use crate::serial::Serial;
use crate::server::CrucibleBackendMap;
pub use nexus_client::Client as NexusClient;

use anyhow::Result;

// Arbitrary ROM limit for now
const MAX_ROM_SIZE: usize = 0x20_0000;

fn open_bootrom<P: AsRef<std::path::Path>>(path: P) -> Result<(File, usize)> {
    let fp = File::open(path.as_ref())?;
    let len = fp.metadata()?.len();
    if len % (PAGE_SIZE as u64) != 0 {
        Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "rom {} length {:x} not aligned to {:x}",
                path.as_ref().to_string_lossy(),
                len,
                PAGE_SIZE
            ),
        )
        .into())
    } else {
        Ok((fp, len as usize))
    }
}

fn get_spec_guest_ram_limits(spec: &InstanceSpecV0) -> (usize, usize) {
    const MB: usize = 1024 * 1024;
    const GB: usize = 1024 * 1024 * 1024;
    let memsize = spec.devices.board.memory_mb as usize * MB;
    let lowmem = memsize.min(3 * GB);
    let highmem = memsize.saturating_sub(3 * GB);
    (lowmem, highmem)
}

pub fn build_instance(
    name: &str,
    spec: &InstanceSpecV0,
    use_reservoir: bool,
    _log: slog::Logger,
) -> Result<Instance> {
    let (lowmem, highmem) = get_spec_guest_ram_limits(spec);
    let create_opts = propolis::vmm::CreateOpts {
        force: true,
        use_reservoir,
        track_dirty: true,
    };
    let mut builder = Builder::new(name, create_opts)?
        .max_cpus(spec.devices.board.cpus)?
        .add_mem_region(0, lowmem, "lowmem")?
        .add_rom_region(0x1_0000_0000 - MAX_ROM_SIZE, MAX_ROM_SIZE, "bootrom")?
        .add_mmio_region(0xc000_0000_usize, 0x2000_0000_usize, "dev32")?
        .add_mmio_region(0xe000_0000_usize, 0x1000_0000_usize, "pcicfg")?;

    let highmem_start = 0x1_0000_0000;
    if highmem > 0 {
        builder = builder.add_mem_region(highmem_start, highmem, "highmem")?;
    }

    let dev64_start = highmem_start + highmem;
    builder = builder.add_mmio_region(
        dev64_start,
        vmm::MAX_PHYSMEM - dev64_start,
        "dev64",
    )?;

    Ok(Instance::create(builder.finalize()?))
}

pub struct RegisteredChipset(Arc<I440Fx>, EntityID);
impl RegisteredChipset {
    pub fn device(&self) -> &Arc<I440Fx> {
        &self.0
    }
}

struct StorageBackendInstance {
    be: Arc<dyn block::Backend>,
    child: inventory::ChildRegister,
    crucible: Option<(uuid::Uuid, Arc<block::CrucibleBackend>)>,
}

pub struct MachineInitializer<'a> {
    log: slog::Logger,
    machine: &'a Machine,
    inv: &'a Inventory,
    spec: &'a InstanceSpecV0,
    producer_registry: Option<ProducerRegistry>,
}

impl<'a> MachineInitializer<'a> {
    pub fn new(
        log: slog::Logger,
        machine: &'a Machine,
        inv: &'a Inventory,
        spec: &'a InstanceSpecV0,
        producer_registry: Option<ProducerRegistry>,
    ) -> Self {
        MachineInitializer { log, machine, inv, spec, producer_registry }
    }

    pub fn initialize_rom<P: AsRef<std::path::Path>>(
        &self,
        path: P,
    ) -> Result<(), Error> {
        let (romfp, rom_len) = open_bootrom(path.as_ref())
            .unwrap_or_else(|e| panic!("Cannot open bootrom: {}", e));

        let mem = self.machine.acc_mem.access().unwrap();
        let mapping = mem.direct_writable_region_by_name("bootrom")?;
        let offset = mapping.len() - rom_len;
        let submapping = mapping.subregion(offset, rom_len).unwrap();
        let nread = submapping.pread(&romfp, rom_len, 0)?;
        if nread != rom_len {
            // TODO: Handle short read
            return Err(Error::new(ErrorKind::InvalidData, "short read"));
        }
        Ok(())
    }

    pub fn initialize_kernel_devs(&self) -> Result<(), Error> {
        let (lowmem, highmem) = get_spec_guest_ram_limits(self.spec);

        let rtc = &self.machine.kernel_devs.rtc;
        rtc.memsize_to_nvram(lowmem as u32, highmem as u64)?;
        rtc.set_time(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time precedes UNIX epoch"),
        )?;

        Ok(())
    }

    pub fn initialize_chipset(
        &self,
        event_handler: &Arc<dyn super::vm::ChipsetEventHandler>,
    ) -> Result<RegisteredChipset, Error> {
        let mut pci_builder = pci::topology::Builder::new();
        for (name, bridge) in &self.spec.devices.pci_pci_bridges {
            let desc = pci::topology::BridgeDescription::new(
                pci::topology::LogicalBusId(bridge.downstream_bus),
                bridge.pci_path.try_into().map_err(|e| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Couldn't get PCI BDF for bridge {}: {}",
                            name, e
                        ),
                    )
                })?,
            );
            pci_builder.add_bridge(desc)?;
        }
        let pci_topology = pci_builder.finish(self.inv, self.machine)?;

        match self.spec.devices.board.chipset {
            instance_spec::components::board::Chipset::I440Fx(i440fx) => {
                let power_ref = Arc::downgrade(event_handler);
                let reset_ref = Arc::downgrade(event_handler);
                let power_pin = Arc::new(propolis::intr_pins::FuncPin::new(
                    Box::new(move |rising| {
                        if rising {
                            if let Some(handler) = power_ref.upgrade() {
                                handler.chipset_halt();
                            }
                        }
                    }),
                ));
                let reset_pin = Arc::new(propolis::intr_pins::FuncPin::new(
                    Box::new(move |rising| {
                        if rising {
                            if let Some(handler) = reset_ref.upgrade() {
                                handler.chipset_reset();
                            }
                        }
                    }),
                ));

                let chipset = I440Fx::create(
                    self.machine,
                    pci_topology,
                    i440fx::Opts {
                        power_pin: Some(power_pin),
                        reset_pin: Some(reset_pin),
                        enable_pcie: i440fx.enable_pcie,
                    },
                    self.log.new(slog::o!("dev" => "chipset")),
                );
                let id = self.inv.register(&chipset)?;
                Ok(RegisteredChipset(chipset, id))
            }
        }
    }

    pub fn initialize_uart(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<Serial<LpcUart>, Error> {
        use instance_spec::components::devices::SerialPortNumber;

        let mut com1 = None;
        for (name, serial_spec) in &self.spec.devices.serial_ports {
            let (irq, port) = match serial_spec.num {
                SerialPortNumber::Com1 => (ibmpc::IRQ_COM1, ibmpc::PORT_COM1),
                SerialPortNumber::Com2 => (ibmpc::IRQ_COM2, ibmpc::PORT_COM2),
                SerialPortNumber::Com3 => (ibmpc::IRQ_COM3, ibmpc::PORT_COM3),
                SerialPortNumber::Com4 => (ibmpc::IRQ_COM4, ibmpc::PORT_COM4),
            };

            let dev = LpcUart::new(chipset.device().irq_pin(irq).unwrap());
            dev.set_autodiscard(true);
            LpcUart::attach(&dev, &self.machine.bus_pio, port);
            self.inv.register_instance(&dev, name)?;
            if matches!(serial_spec.num, SerialPortNumber::Com1) {
                assert!(com1.is_none());
                com1 = Some(dev);
            }
        }

        let sink_size = NonZeroUsize::new(64).unwrap();
        let source_size = NonZeroUsize::new(1024).unwrap();
        Ok(Serial::new(com1.unwrap(), sink_size, source_size))
    }

    pub fn initialize_ps2(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<EntityID, Error> {
        let ps2_ctrl = PS2Ctrl::create();
        ps2_ctrl.attach(&self.machine.bus_pio, chipset.device().as_ref());
        let id = self.inv.register(&ps2_ctrl)?;
        Ok(id)
    }

    pub fn initialize_qemu_debug_port(&self) -> Result<(), Error> {
        let dbg = QemuDebugPort::create(&self.machine.bus_pio);
        let debug_file = std::fs::File::create("debug.out")?;
        let poller = chardev::BlockingFileOutput::new(debug_file);
        poller.attach(Arc::clone(&dbg) as Arc<dyn BlockingSource>);
        self.inv.register(&dbg)?;
        Ok(())
    }

    fn create_storage_backend_from_spec(
        &self,
        backend_spec: &instance_spec::v0::StorageBackendV0,
        backend_name: &str,
        nexus_client: &Option<NexusClient>,
    ) -> Result<StorageBackendInstance, Error> {
        match backend_spec {
            instance_spec::v0::StorageBackendV0::Crucible(spec) => {
                info!(self.log, "Creating Crucible disk";
                      "serialized_vcr" => &spec.request_json);

                let vcr: VolumeConstructionRequest =
                    serde_json::from_str(&spec.request_json)?;

                let cru_id = match vcr {
                    VolumeConstructionRequest::Volume { id, .. } => {
                        id.to_string()
                    }
                    VolumeConstructionRequest::File { id, .. } => {
                        id.to_string()
                    }
                    VolumeConstructionRequest::Url { id, .. } => id.to_string(),
                    VolumeConstructionRequest::Region { .. } => {
                        "Region".to_string()
                    }
                };

                let be = propolis::block::CrucibleBackend::create(
                    vcr,
                    spec.readonly,
                    self.producer_registry.clone(),
                    nexus_client.clone(),
                    self.log.new(
                        slog::o!("component" => format!("crucible-{cru_id}")),
                    ),
                )?;

                let child = inventory::ChildRegister::new(
                    &be,
                    Some(be.get_uuid()?.to_string()),
                );

                let crucible = Some((be.get_uuid()?, be.clone()));
                Ok(StorageBackendInstance { be, child, crucible })
            }
            instance_spec::v0::StorageBackendV0::File(spec) => {
                info!(self.log, "Creating file disk backend";
                      "path" => &spec.path);

                let nworkers = NonZeroUsize::new(8).unwrap();
                let be = propolis::block::FileBackend::create(
                    &spec.path,
                    spec.readonly,
                    nworkers,
                    self.log.new(
                        slog::o!("component" => format!("file-{}", spec.path)),
                    ),
                )?;

                let child =
                    inventory::ChildRegister::new(&be, Some(spec.path.clone()));
                Ok(StorageBackendInstance { be, child, crucible: None })
            }
            instance_spec::v0::StorageBackendV0::InMemory(spec) => {
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    &spec.base64,
                )
                .map_err(|e| {
                    Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "failed to decode base64 contents of in-memory \
                                disk: {}",
                            e
                        ),
                    )
                })?;

                info!(self.log, "Creating in-memory disk backend";
                      "len" => bytes.len());

                let be = propolis::block::InMemoryBackend::create(
                    bytes.clone(),
                    spec.readonly,
                    512,
                )?;

                let child = inventory::ChildRegister::new(
                    &be,
                    Some(backend_name.to_string()),
                );

                Ok(StorageBackendInstance { be, child, crucible: None })
            }
        }
    }

    /// Initializes the storage devices and backends listed in this
    /// initializer's instance spec.
    ///
    /// On success, returns a map from Crucible backend IDs to Crucible
    /// backends.
    pub fn initialize_storage_devices(
        &self,
        chipset: &RegisteredChipset,
        nexus_client: Option<NexusClient>,
    ) -> Result<CrucibleBackendMap, Error> {
        enum DeviceInterface {
            Virtio,
            Nvme,
        }

        let mut crucible_backends: CrucibleBackendMap = Default::default();
        for (name, device_spec) in &self.spec.devices.storage_devices {
            info!(
                self.log,
                "Creating storage device {} with properties {:?}",
                name,
                device_spec
            );

            let (device_interface, backend_name, pci_path) = match device_spec {
                instance_spec::v0::StorageDeviceV0::VirtioDisk(disk) => {
                    (DeviceInterface::Virtio, &disk.backend_name, disk.pci_path)
                }
                instance_spec::v0::StorageDeviceV0::NvmeDisk(disk) => {
                    (DeviceInterface::Nvme, &disk.backend_name, disk.pci_path)
                }
            };

            let backend_spec = self
                .spec
                .backends
                .storage_backends
                .get(backend_name)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Backend {} not found for storage device {}",
                            backend_name, name
                        ),
                    )
                })?;

            let StorageBackendInstance { be: backend, child, crucible } = self
                .create_storage_backend_from_spec(
                    backend_spec,
                    &backend_name,
                    &nexus_client,
                )?;

            let bdf: pci::Bdf = pci_path.try_into().map_err(|e| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "Couldn't get PCI BDF for storage device {}: {}",
                        name, e
                    ),
                )
            })?;

            let be_info = backend.info();
            match device_interface {
                DeviceInterface::Virtio => {
                    let vioblk = virtio::PciVirtioBlock::new(0x100, be_info);
                    let id =
                        self.inv.register_instance(&vioblk, bdf.to_string())?;
                    let _ = self.inv.register_child(child, id).unwrap();
                    backend.attach(vioblk.clone())?;
                    chipset.device().pci_attach(bdf, vioblk);
                }
                DeviceInterface::Nvme => {
                    let nvme = nvme::PciNvme::create(
                        name.to_string(),
                        be_info,
                        self.log.new(
                            slog::o!("component" => format!("nvme-{}", name)),
                        ),
                    );
                    let id =
                        self.inv.register_instance(&nvme, bdf.to_string())?;
                    let _ = self.inv.register_child(child, id).unwrap();
                    backend.attach(nvme.clone())?;
                    chipset.device().pci_attach(bdf, nvme);
                }
            };
            if let Some((id, backend)) = crucible {
                let prev = crucible_backends.insert(id, backend);
                if prev.is_some() {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("multiple disks with id {}", id),
                    ));
                }
            }
        }
        Ok(crucible_backends)
    }

    pub fn initialize_network_devices(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<(), Error> {
        for (name, vnic_spec) in &self.spec.devices.network_devices {
            info!(self.log, "Creating vNIC {}", name);
            let instance_spec::v0::NetworkDeviceV0::VirtioNic(vnic_spec) =
                vnic_spec;

            let backend_spec = self
                .spec
                .backends
                .network_backends
                .get(&vnic_spec.backend_name)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Backend {} not found for vNIC {}",
                            vnic_spec.backend_name, name
                        ),
                    )
                })?;
            let bdf: pci::Bdf = vnic_spec.pci_path.try_into().map_err(|e| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("Couldn't get PCI BDF for vNIC {}: {}", name, e),
                )
            })?;

            let vnic_name = match backend_spec {
                instance_spec::v0::NetworkBackendV0::Virtio(spec) => {
                    &spec.vnic_name
                }
                instance_spec::v0::NetworkBackendV0::Dlpi(_) => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Network backend must be virtio for vNIC {}",
                            name,
                        ),
                    ));
                }
            };

            let viona = virtio::PciVirtioViona::new(
                vnic_name,
                0x100,
                &self.machine.hdl,
            )?;
            let _ = self.inv.register_instance(&viona, bdf.to_string())?;
            chipset.device().pci_attach(bdf, viona);
        }
        Ok(())
    }

    #[cfg(feature = "falcon")]
    pub fn initialize_softnpu_ports(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<(), Error> {
        // Check to make sure we actually have both a pci port and at least one
        // regular SoftNpu port, otherwise just return.
        let pci_port = match &self.spec.devices.softnpu_pci_port {
            Some(tfp) => tfp,
            None => return Ok(()),
        };
        if self.spec.devices.softnpu_ports.is_empty() {
            return Ok(());
        }

        let ports: Vec<&SoftNpuPort> =
            self.spec.devices.softnpu_ports.values().collect();

        let mut data_links: Vec<String> = Vec::new();
        for x in &ports {
            let backend = self
                .spec
                .backends
                .network_backends
                .get(&x.backend_name)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Backend {} not found for softnpu port",
                            x.backend_name
                        ),
                    )
                })?;

            let vnic = match &backend.kind {
                NetworkBackendKind::Dlpi { vnic_name } => vnic_name,
                _ => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Softnpu port must have DLPI backend: {}",
                            x.backend_name
                        ),
                    ));
                }
            };
            data_links.push(vnic.clone());
        }

        // Set up an LPC uart for ASIC management comms from the guest.
        //
        // NOTE: SoftNpu squats on com4.
        let pio = &self.machine.bus_pio;
        let port = ibmpc::PORT_COM4;
        let uart =
            LpcUart::new(chipset.device().irq_pin(ibmpc::IRQ_COM4).unwrap());
        uart.set_autodiscard(true);
        LpcUart::attach(&uart, pio, port);
        self.inv.register_instance(&uart, "softnpu-uart")?;

        // Start with no pipeline. The guest must load the initial P4 program.
        let pipeline = Arc::new(std::sync::Mutex::new(None));

        // Set up the p9fs device for guest programs to load P4 programs
        // through.
        let p9_handler = virtio::softnpu::SoftNpuP9Handler::new(
            "/dev/softnpufs".to_owned(),
            "/dev/softnpufs".to_owned(),
            self.spec.devices.softnpu_ports.len() as u16,
            pipeline.clone(),
            self.log.clone(),
        );
        let vio9p =
            virtio::p9fs::PciVirtio9pfs::new(0x40, Arc::new(p9_handler));
        self.inv.register_instance(&vio9p, "softnpu-p9fs")?;
        let bdf: pci::Bdf = self
            .spec
            .devices
            .softnpu_p9
            .as_ref()
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "SoftNpu p9 device missing".to_owned(),
                )
            })?
            .pci_path
            .try_into()
            .map_err(|e| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "Couldn't get PCI BDF for SoftNpu p9 device: {}",
                        e
                    ),
                )
            })?;
        chipset.device().pci_attach(bdf, vio9p.clone());

        // Create the SoftNpu device.
        let queue_size = 0x8000;
        let softnpu = virtio::softnpu::SoftNpu::new(
            data_links,
            queue_size,
            uart,
            vio9p,
            pipeline,
            self.log.clone(),
        )
        .map_err(|e| -> std::io::Error {
            let io_err: std::io::Error = e;
            std::io::Error::new(
                io_err.kind(),
                format!("register softnpu: {}", io_err),
            )
        })?;
        self.inv
            .register(&softnpu)
            .map_err(|e| -> std::io::Error { e.into() })?;

        // Create the SoftNpu PCI port.
        let bdf: pci::Bdf = pci_port.pci_path.try_into().map_err(|e| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("Couldn't get PCI BDF for SoftNpu pci port: {}", e),
            )
        })?;
        self.inv
            .register_instance(&softnpu.pci_port, bdf.to_string())
            .map_err(|e| -> std::io::Error {
                let io_err: std::io::Error = e.into();
                std::io::Error::new(
                    io_err.kind(),
                    format!("register softnpu port: {}", io_err),
                )
            })?;
        chipset.device().pci_attach(bdf, softnpu.pci_port.clone());

        Ok(())
    }

    #[cfg(feature = "falcon")]
    pub fn initialize_9pfs(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<(), Error> {
        // Check that there is actually a p9fs device to register, if not bail
        // early.
        let p9fs = match &self.spec.devices.p9fs {
            Some(p9fs) => p9fs,
            None => return Ok(()),
        };

        let bdf: pci::Bdf = p9fs.pci_path.try_into().map_err(|e| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("Couldn't get PCI BDF for p9fs device: {}", e),
            )
        })?;

        let handler = virtio::p9fs::HostFSHandler::new(
            p9fs.source.to_owned(),
            p9fs.target.to_owned(),
            p9fs.chunk_size,
            self.log.clone(),
        );
        let vio9p = virtio::p9fs::PciVirtio9pfs::new(0x40, Arc::new(handler));
        self.inv
            .register(&vio9p)
            .map_err(|e| -> std::io::Error { e.into() })?;

        chipset.device().pci_attach(bdf, vio9p);
        Ok(())
    }

    pub fn initialize_fwcfg(&self, cpus: u8) -> Result<EntityID, Error> {
        let mut fwcfg = fwcfg::FwCfgBuilder::new();
        fwcfg
            .add_legacy(
                fwcfg::LegacyId::SmpCpuCount,
                fwcfg::FixedItem::new_u32(cpus as u32),
            )
            .unwrap();

        let ramfb = ramfb::RamFb::create(
            self.log.new(slog::o!("component" => "ramfb")),
        );
        ramfb.attach(&mut fwcfg, &self.machine.acc_mem);

        let fwcfg_dev = fwcfg.finalize();
        fwcfg_dev.attach(&self.machine.bus_pio, &self.machine.acc_mem);

        self.inv.register(&fwcfg_dev)?;
        let ramfb_id = self.inv.register(&ramfb)?;
        Ok(ramfb_id)
    }

    pub fn initialize_cpus(&self) -> Result<(), Error> {
        for vcpu in self.machine.vcpus.iter() {
            vcpu.set_default_capabs().unwrap();
        }
        Ok(())
    }
}
