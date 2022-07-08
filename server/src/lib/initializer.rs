use std::convert::TryInto;
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::SystemTime;

use propolis::block;
use propolis::chardev::{self, BlockingSource, Source};
use propolis::common::PAGE_SIZE;
use propolis::dispatch::Dispatcher;
use propolis::hw::chipset::i440fx;
use propolis::hw::chipset::i440fx::I440Fx;
use propolis::hw::chipset::Chipset;
use propolis::hw::ibmpc;
use propolis::hw::pci;
use propolis::hw::ps2ctrl::PS2Ctrl;
use propolis::hw::qemu::{debug::QemuDebugPort, fwcfg, ramfb};
use propolis::hw::uart::LpcUart;
use propolis::hw::{nvme, virtio};
use propolis::instance::Instance;
use propolis::inventory::{self, EntityID, Inventory};
use propolis::vmm::{self, Builder, Machine, MachineCtx, Prot};
use propolis_client::instance_spec::{self, *};
use slog::info;

use crate::serial::Serial;
use crate::server::CrucibleBackendMap;

use anyhow::Result;
use tokio::runtime::Handle;

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
    let memsize = spec.board.memory_mb as usize * MB;
    let lowmem = memsize.min(3 * GB);
    let highmem = memsize.saturating_sub(3 * GB);
    (lowmem, highmem)
}

pub fn build_instance(
    name: &str,
    spec: &InstanceSpec,
    use_reservoir: bool,
    log: slog::Logger,
) -> Result<Arc<Instance>> {
    let (lowmem, highmem) = get_spec_guest_ram_limits(spec);
    let create_opts = propolis::vmm::CreateOpts { force: true, use_reservoir };
    let mut builder = Builder::new(name, create_opts)?
        .max_cpus(spec.board.cpus)?
        .add_mem_region(0, lowmem, Prot::ALL, "lowmem")?
        .add_rom_region(
            0x1_0000_0000 - MAX_ROM_SIZE,
            MAX_ROM_SIZE,
            Prot::READ | Prot::EXEC,
            "bootrom",
        )?
        .add_mmio_region(0xc000_0000_usize, 0x2000_0000_usize, "dev32")?
        .add_mmio_region(0xe000_0000_usize, 0x1000_0000_usize, "pcicfg")?
        .add_mmio_region(
            vmm::MAX_SYSMEM,
            vmm::MAX_PHYSMEM - vmm::MAX_SYSMEM,
            "dev64",
        )?;
    if highmem > 0 {
        builder = builder.add_mem_region(
            0x1_0000_0000,
            highmem,
            Prot::ALL,
            "highmem",
        )?;
    }

    // Allow propolis to use the existing tokio runtime for spawning and
    // dispatching its tasks
    let rt_handle = Some(Handle::current());
    let inst = Instance::create(builder.finalize()?, rt_handle, Some(log))?;
    inst.spawn_vcpu_workers(propolis::vcpu_run_loop)?;
    Ok(inst)
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
    mctx: &'a MachineCtx,
    disp: &'a Dispatcher,
    inv: &'a Inventory,
    spec: &'a InstanceSpec,
}

impl<'a> MachineInitializer<'a> {
    pub fn new(
        log: slog::Logger,
        machine: &'a Machine,
        mctx: &'a MachineCtx,
        disp: &'a Dispatcher,
        inv: &'a Inventory,
        spec: &'a InstanceSpec,
    ) -> Self {
        MachineInitializer { log, machine, mctx, disp, inv, spec }
    }

    pub fn initialize_rom<P: AsRef<std::path::Path>>(
        &self,
        path: P,
    ) -> Result<(), Error> {
        let (romfp, rom_len) = open_bootrom(path.as_ref())
            .unwrap_or_else(|e| panic!("Cannot open bootrom: {}", e));
        self.machine.populate_rom("bootrom", |mapping| {
            let mapping = mapping.as_ref();
            if mapping.len() < rom_len {
                return Err(Error::new(ErrorKind::InvalidData, "rom too long"));
            }
            let offset = mapping.len() - rom_len;
            let submapping = mapping.subregion(offset, rom_len).unwrap();
            let nread = submapping.pread(&romfp, rom_len, 0)?;
            if nread != rom_len {
                // TODO: Handle short read
                return Err(Error::new(ErrorKind::InvalidData, "short read"));
            }
            Ok(())
        })?;
        Ok(())
    }

    pub fn initialize_kernel_devs(&self) -> Result<(), Error> {
        let (lowmem, highmem) = get_spec_guest_ram_limits(self.spec);
        let hdl = self.mctx.hdl();

        let rtc = &self.machine.kernel_devs.rtc;
        rtc.memsize_to_nvram(lowmem, highmem, hdl)?;
        rtc.set_time(SystemTime::now(), hdl)?;

        Ok(())
    }

    pub fn initialize_chipset(&self) -> Result<RegisteredChipset, Error> {
        let mut pci_builder = pci::topology::Builder::new();
        for (name, bridge) in &self.spec.pci_pci_bridges {
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
        let pci_topology = pci_builder.finish(
            &self.inv,
            &self.machine.bus_pio,
            &self.machine.bus_mmio,
        )?;

        match self.spec.board.chipset {
            instance_spec::Chipset::I440Fx { enable_pcie } => {
                let chipset = I440Fx::create(
                    self.machine,
                    pci_topology,
                    i440fx::CreateOptions { enable_pcie },
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
        let pio = self.mctx.pio();
        let mut com1 = None;
        for (name, serial_spec) in &self.spec.serial_ports {
            let (irq, port) = match serial_spec.num {
                SerialPortNumber::Com1 => (ibmpc::IRQ_COM1, ibmpc::PORT_COM1),
                SerialPortNumber::Com2 => (ibmpc::IRQ_COM2, ibmpc::PORT_COM2),
                SerialPortNumber::Com3 => (ibmpc::IRQ_COM3, ibmpc::PORT_COM3),
                SerialPortNumber::Com4 => (ibmpc::IRQ_COM4, ibmpc::PORT_COM4),
            };

            let dev = LpcUart::new(chipset.device().irq_pin(irq).unwrap());
            dev.set_autodiscard(true);
            LpcUart::attach(&dev, pio, port);
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
    ) -> Result<(), Error> {
        let pio = self.mctx.pio();
        let ps2_ctrl = PS2Ctrl::create();
        ps2_ctrl.attach(pio, chipset.device().as_ref());
        self.inv.register(&ps2_ctrl)?;
        Ok(())
    }

    pub fn initialize_qemu_debug_port(&self) -> Result<(), Error> {
        let dbg = QemuDebugPort::create(self.mctx.pio());
        let debug_file = std::fs::File::create("debug.out")?;
        let poller = chardev::BlockingFileOutput::new(debug_file);
        poller.attach(Arc::clone(&dbg) as Arc<dyn BlockingSource>, self.disp);
        self.inv.register(&dbg)?;
        Ok(())
    }

    fn initialize_storage_backend(
        &self,
        name: &str,
        backend_spec: &StorageBackend,
    ) -> Result<StorageBackendInstance, Error> {
        Ok(match &backend_spec.kind {
            StorageBackendKind::Crucible { gen, req } => {
                info!(
                    self.log,
                    "Creating Crucible disk from request {}", req.json
                );
                let be = propolis::block::CrucibleBackend::create(
                    *gen,
                    req.try_into().map_err(|e| {
                        Error::new(
                            ErrorKind::InvalidData,
                            format!(
                                "Failed to deserialize Crucible volume \
                                   construction request: {}",
                                e
                            ),
                        )
                    })?,
                    backend_spec.readonly,
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
                )?;
                let child =
                    inventory::ChildRegister::new(&be, Some(path.to_string()));
                StorageBackendInstance { be, child, crucible: None }
            }
            StorageBackendKind::InMemory { bytes } => {
                info!(
                    self.log,
                    "Creating in-memory disk backend from {} bytes",
                    bytes.len()
                );
                let be = propolis::block::InMemoryBackend::create(
                    bytes.to_vec(),
                    backend_spec.readonly,
                    512,
                )?;
                let child =
                    inventory::ChildRegister::new(&be, Some(name.to_string()));
                StorageBackendInstance { be, child, crucible: None }
            }
        })
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
        for (name, device_spec) in &self.spec.storage_devices {
            info!(
                self.log,
                "Creating storage device {} of kind {:?}",
                name,
                device_spec.kind
            );
            let backend_spec = self
                .spec
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
            let StorageBackendInstance { be: backend, child, crucible } = self
                .initialize_storage_backend(
                    &device_spec.backend_name,
                    &backend_spec,
                )?;
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
                    backend.attach(vioblk.clone(), self.disp)?;
                    chipset.device().pci_attach(bdf, vioblk);
                }
                StorageDeviceKind::Nvme => {
                    let nvme = nvme::PciNvme::create(name.to_string(), be_info);
                    let id =
                        self.inv.register_instance(&nvme, bdf.to_string())?;
                    let _ = self.inv.register_child(child, id).unwrap();
                    backend.attach(nvme.clone(), self.disp)?;
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
        for (name, vnic_spec) in &self.spec.network_devices {
            info!(self.log, "Creating vNIC {}", name);
            let backend_spec = self
                .spec
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
            let hdl = self.machine.get_hdl();
            let viona = virtio::PciVirtioViona::new(
                match &backend_spec.kind {
                    NetworkBackendKind::Virtio { vnic_name } => vnic_name
                },
                0x100,
                &hdl,
            )?;
            let _ = self.inv.register_instance(&viona, bdf.to_string())?;
            chipset.device().pci_attach(bdf, viona);
        }
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

        let ramfb = ramfb::RamFb::create();
        ramfb.attach(&mut fwcfg);

        let fwcfg_dev = fwcfg.finalize();
        let pio = self.mctx.pio();
        fwcfg_dev.attach(pio);

        self.inv.register(&fwcfg_dev)?;
        let ramfb_id = self.inv.register(&ramfb)?;
        Ok(ramfb_id)
    }

    pub fn initialize_cpus(&self) -> Result<(), Error> {
        for vcpu in self.mctx.vcpus() {
            vcpu.set_default_capabs().unwrap();
        }
        Ok(())
    }
}
