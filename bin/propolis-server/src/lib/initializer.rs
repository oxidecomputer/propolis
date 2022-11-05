use std::convert::TryInto;
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::SystemTime;

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
use propolis_client::instance_spec::{self, *};
use slog::info;

use crate::serial::Serial;
use crate::server::CrucibleBackendMap;

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

fn get_spec_guest_ram_limits(spec: &InstanceSpec) -> (usize, usize) {
    const MB: usize = 1024 * 1024;
    const GB: usize = 1024 * 1024 * 1024;
    let memsize = spec.devices.board.memory_mb as usize * MB;
    let lowmem = memsize.min(3 * GB);
    let highmem = memsize.saturating_sub(3 * GB);
    (lowmem, highmem)
}

pub fn build_instance(
    name: &str,
    spec: &InstanceSpec,
    use_reservoir: bool,
    _log: slog::Logger,
) -> Result<Arc<Instance>> {
    let (lowmem, highmem) = get_spec_guest_ram_limits(spec);
    let create_opts = propolis::vmm::CreateOpts { force: true, use_reservoir };
    let mut builder = Builder::new(name, create_opts)?
        .max_cpus(spec.devices.board.cpus)?
        .add_mem_region(0, lowmem, "lowmem")?
        .add_rom_region(0x1_0000_0000 - MAX_ROM_SIZE, MAX_ROM_SIZE, "bootrom")?
        .add_mmio_region(0xc000_0000_usize, 0x2000_0000_usize, "dev32")?
        .add_mmio_region(0xe000_0000_usize, 0x1000_0000_usize, "pcicfg")?
        .add_mmio_region(
            vmm::MAX_SYSMEM,
            vmm::MAX_PHYSMEM - vmm::MAX_SYSMEM,
            "dev64",
        )?;
    if highmem > 0 {
        builder = builder.add_mem_region(0x1_0000_0000, highmem, "highmem")?;
    }

    Ok(Arc::new(Instance::create(builder.finalize()?)))
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
    spec: &'a InstanceSpec,
    producer_registry: Option<ProducerRegistry>,
}

impl<'a> MachineInitializer<'a> {
    pub fn new(
        log: slog::Logger,
        machine: &'a Machine,
        inv: &'a Inventory,
        spec: &'a InstanceSpec,
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
        rtc.set_time(SystemTime::now())?;

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
            instance_spec::Chipset::I440Fx { enable_pcie } => {
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
                        enable_pcie,
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

    /// Initializes the storage devices and backends listed in this
    /// initializer's instance spec.
    ///
    /// On success, returns a map from Crucible backend IDs to Crucible
    /// backends.
    pub fn initialize_storage_devices(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<CrucibleBackendMap, Error> {
        let mut crucible_backends: CrucibleBackendMap = Default::default();
        for (name, device_spec) in &self.spec.devices.storage_devices {
            info!(
                self.log,
                "Creating storage device {} of kind {:?}",
                name,
                device_spec.kind
            );
            let backend_spec = self
                .spec
                .backends
                .storage_backends
                .get(&device_spec.backend_name)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Backend {} not found for storage device {}",
                            device_spec.backend_name, name
                        ),
                    )
                })?;
            let StorageBackendInstance { be: backend, child, crucible } =
                match &backend_spec.kind {
                    StorageBackendKind::Crucible { gen, req } => {
                        info!(
                            self.log,
                            "Creating Crucible disk from request {:?}", req
                        );
                        let be = propolis::block::CrucibleBackend::create(
                            *gen,
                            req.clone(),
                            backend_spec.readonly,
                            self.producer_registry.clone(),
                        )?;
                        let child = inventory::ChildRegister::new(
                            &be,
                            Some(be.get_uuid()?.to_string()),
                        );
                        let crucible = Some((be.get_uuid()?, be.clone()));
                        StorageBackendInstance { be, child, crucible }
                    }
                    StorageBackendKind::File { path } => {
                        info!(
                            self.log,
                            "Creating file disk backend using path {}", path
                        );
                        let nworkers = NonZeroUsize::new(8).unwrap();
                        let be = propolis::block::FileBackend::create(
                            path,
                            backend_spec.readonly,
                            nworkers,
                            self.log.new(slog::o!("component" =>
                                              format!("file-{}", path))),
                        )?;
                        let child = inventory::ChildRegister::new(
                            &be,
                            Some(path.to_string()),
                        );
                        StorageBackendInstance { be, child, crucible: None }
                    }
                    StorageBackendKind::InMemory { base64 } => {
                        let bytes = base64::decode(base64).map_err(|e| {
                            Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!(
                                    "failed to decode base64 contents of \
                                     in-memory disk: {}",
                                    e
                                ),
                            )
                        })?;
                        info!(
                            self.log,
                            "Creating in-memory disk backend from {} bytes",
                            bytes.len()
                        );
                        let be = propolis::block::InMemoryBackend::create(
                            bytes.clone(),
                            backend_spec.readonly,
                            512,
                        )?;
                        let child = inventory::ChildRegister::new(
                            &be,
                            Some(name.to_string()),
                        );
                        StorageBackendInstance { be, child, crucible: None }
                    }
                };

            let bdf: pci::Bdf =
                device_spec.pci_path.try_into().map_err(|e| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Couldn't get PCI BDF for storage device {}: {}",
                            name, e
                        ),
                    )
                })?;
            let be_info = backend.info();
            match device_spec.kind {
                StorageDeviceKind::Virtio => {
                    let vioblk = virtio::PciVirtioBlock::new(0x100, be_info);
                    let id =
                        self.inv.register_instance(&vioblk, bdf.to_string())?;
                    let _ = self.inv.register_child(child, id).unwrap();
                    backend.attach(vioblk.clone())?;
                    chipset.device().pci_attach(bdf, vioblk);
                }
                StorageDeviceKind::Nvme => {
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
            let viona = virtio::PciVirtioViona::new(
                match &backend_spec.kind {
                    NetworkBackendKind::Virtio { vnic_name } => vnic_name,
                },
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
        let tfport0 = match &self.spec.devices.tfport0 {
            Some(tfp) => tfp,
            None => return Ok(()),
        };

        let ports: Vec<&SoftNpuPort> =
            self.spec.devices.softnpu_ports.values().collect();

        let data_links: Vec<String> =
            ports.iter().map(|x| x.vnic.clone()).collect();

        if ports.is_empty() {
            return Ok(());
        }

        let queue_size = 0x8000;

        // TODO squatting on com4???
        let pio = &self.machine.bus_pio;
        let port = ibmpc::PORT_COM4;
        let uart =
            LpcUart::new(chipset.device().irq_pin(ibmpc::IRQ_COM4).unwrap());
        uart.set_autodiscard(true);
        LpcUart::attach(&uart, pio, port);
        self.inv.register_instance(&uart, "softnpu-uart")?;

        let pipeline = Arc::new(tokio::sync::Mutex::new(None));

        let p9_handler = virtio::SoftNPUP9Handler::new(
            "/dev/softnpufs".to_owned(),
            "/dev/softnpufs".to_owned(),
            pipeline.clone(),
            self.log.clone(),
        );
        let vio9p = virtio::PciVirtio9pfs::new(0x40, p9_handler);
        self.inv.register_instance(&vio9p, "softnpu-p9fs")?;
        let bdf: pci::Bdf = self
            .spec
            .devices
            .softnpu_p9
            .as_ref()
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "SoftNPU p9 device missing".to_owned(),
                )
            })?
            .pci_path
            .try_into()
            .map_err(|e| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "Couldn't get PCI BDF for SoftNPU p9 device: {}",
                        e
                    ),
                )
            })?;
        chipset.device().pci_attach(bdf, vio9p.clone());

        let softnpu = virtio::SoftNPU::new(
            data_links,
            queue_size,
            uart,
            vio9p,
            pipeline,
            self.log.clone(),
        )
        .map_err(|e| -> std::io::Error {
            let io_err: std::io::Error = e.into();
            std::io::Error::new(
                io_err.kind(),
                format!("register softnpu: {}", io_err),
            )
        })?;

        self.inv
            .register(&softnpu)
            .map_err(|e| -> std::io::Error { e.into() })?;

        let bdf: pci::Bdf = tfport0.pci_path.try_into().map_err(|e| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("Couldn't get PCI BDF for SoftNPU tfport0: {}", e),
            )
        })?;
        self.inv.register_instance(&softnpu.tfport0, bdf.to_string()).map_err(
            |e| -> std::io::Error {
                let io_err: std::io::Error = e.into();
                std::io::Error::new(
                    io_err.kind(),
                    format!("register softnpu port: {}", io_err),
                )
            },
        )?;
        chipset.device().pci_attach(bdf, softnpu.tfport0.clone());

        Ok(())
    }

    #[cfg(feature = "falcon")]
    pub fn initialize_9pfs(
        &self,
        chipset: &RegisteredChipset,
        source: &str,
        target: &str,
        chunk_size: u32,
        bdf: pci::Bdf,
    ) -> Result<(), Error> {
        let handler = virtio::HostFSHandler::new(
            source.to_owned(),
            target.to_owned(),
            chunk_size,
        );
        let vio9p = virtio::PciVirtio9pfs::new(0x40, handler);
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
