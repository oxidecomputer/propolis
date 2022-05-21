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
use propolis::inventory::{ChildRegister, EntityID, Inventory};
use propolis::vmm::{self, Builder, Machine, MachineCtx, Prot};
use slog::info;

use crate::config;
use crate::serial::Serial;

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

pub fn build_instance(
    name: &str,
    max_cpu: u8,
    lowmem: usize,
    highmem: usize,
    log: slog::Logger,
) -> Result<Arc<Instance>> {
    let mut builder = Builder::new(name, true)?
        .max_cpus(max_cpu)?
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

const PCI_HELPER_EMPTY_ENTRY: Option<Arc<pci::Bus>> = None;
pub struct PciHelper<'a> {
    buses: [Option<Arc<pci::Bus>>; 256],
    chipset: &'a RegisteredChipset,
}

impl<'a> PciHelper<'a> {
    pub fn new(chipset: &'a RegisteredChipset) -> Self {
        Self { buses: [PCI_HELPER_EMPTY_ENTRY; 256], chipset }
    }

    pub fn add_bus(
        &mut self,
        n: pci::BusNum,
        bus: Arc<pci::Bus>,
    ) -> Result<(), Error> {
        assert_ne!(n.get(), 0);
        if self.buses[n.get() as usize].is_some() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Bus {} already registered", n.get()),
            ));
        }
        self.buses[n.get() as usize] = Some(bus);
        Ok(())
    }

    pub fn attach_device(
        &self,
        bdf: pci::Bdf,
        dev: Arc<dyn pci::Endpoint>,
    ) -> Result<(), Error> {
        let bus = match bdf.bus.get() {
            0 => Some(self.chipset.device().pci_root_bus()),
            _ => {
                self.buses[bdf.bus.get() as usize].as_ref().map(|b| b.as_ref())
            }
        }
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("Can't register device at {}: no bus", bdf),
            )
        })?;
        self.chipset.device().pci_attach(bus, bdf.location, dev);
        Ok(())
    }
}

pub struct MachineInitializer<'a> {
    log: slog::Logger,
    machine: &'a Machine,
    mctx: &'a MachineCtx,
    disp: &'a Dispatcher,
    inv: &'a Inventory,
}

impl<'a> MachineInitializer<'a> {
    pub fn new(
        log: slog::Logger,
        machine: &'a Machine,
        mctx: &'a MachineCtx,
        disp: &'a Dispatcher,
        inv: &'a Inventory,
    ) -> Self {
        MachineInitializer { log, machine, mctx, disp, inv }
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

    pub fn initialize_kernel_devs(
        &self,
        lowmem: usize,
        highmem: usize,
    ) -> Result<(), Error> {
        let hdl = self.mctx.hdl();

        let (pic, pit, hpet, ioapic, rtc) = propolis::hw::bhyve::defaults();
        rtc.memsize_to_nvram(lowmem, highmem, hdl)?;
        rtc.set_time(SystemTime::now(), hdl)?;

        self.inv.register(&pic)?;
        self.inv.register(&pit)?;
        self.inv.register(&hpet)?;
        self.inv.register(&ioapic)?;
        self.inv.register(&rtc)?;

        Ok(())
    }

    pub fn initialize_chipset(
        &self,
        config: &config::Chipset,
    ) -> Result<RegisteredChipset, Error> {
        let enable_pcie = config.options.get("enable-pcie").map_or_else(
            || Ok(false),
            |v| {
                v.as_bool().ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidData,
                        format!("invalid value {} for enable-pcie", v),
                    )
                })
            },
        )?;
        let chipset =
            I440Fx::create(self.machine, i440fx::CreateOptions { enable_pcie });
        let id = self.inv.register(&chipset)?;
        Ok(RegisteredChipset(chipset, id))
    }

    pub fn initialize_pci_bridge(
        &self,
        chipset: &RegisteredChipset,
        pci_helper: &mut PciHelper,
        config: &config::PciBridge,
    ) -> Result<Arc<pci::bridge::Bridge>, Error> {
        let bus_num = pci::BusNum::new(config.downstream_bus).unwrap();
        let bus = Arc::new(pci::Bus::new(
            &self.machine.bus_pio,
            &self.machine.bus_mmio,
        ));
        pci_helper.add_bus(bus_num, bus.clone())?;
        let bridge = pci::bridge::Bridge::new(
            pci::bits::BRIDGE_VENDOR_ID,
            pci::bits::BRIDGE_DEVICE_ID,
            bus,
            chipset.device().pci_router().clone(),
        );
        self.inv.register_instance(&bridge, config.pci_path.to_string())?;
        Ok(bridge)
    }

    pub fn initialize_uart(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<Serial<LpcUart>, Error> {
        let uarts = vec![
            (ibmpc::IRQ_COM1, ibmpc::PORT_COM1, "com1"),
            (ibmpc::IRQ_COM2, ibmpc::PORT_COM2, "com2"),
            (ibmpc::IRQ_COM3, ibmpc::PORT_COM3, "com3"),
            (ibmpc::IRQ_COM4, ibmpc::PORT_COM4, "com4"),
        ];
        let pio = self.mctx.pio();
        let mut com1 = None;
        for (irq, port, name) in uarts.iter() {
            let dev = LpcUart::new(chipset.device().irq_pin(*irq).unwrap());
            dev.set_autodiscard(true);
            LpcUart::attach(&dev, pio, *port);
            self.inv.register_instance(&dev, name)?;
            if com1.is_none() {
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
        let poller = chardev::BlockingFileOutput::new(debug_file)?;
        poller.attach(Arc::clone(&dbg) as Arc<dyn BlockingSource>, self.disp);
        self.inv.register(&dbg)?;
        Ok(())
    }

    pub fn initialize_virtio_block(
        &self,
        pci_helper: &PciHelper,
        bdf: pci::Bdf,
        backend: Arc<dyn block::Backend>,
        be_register: ChildRegister,
    ) -> Result<(), Error> {
        let be_info = backend.info();
        let vioblk = virtio::PciVirtioBlock::new(0x100, be_info);
        let id = self.inv.register_instance(&vioblk, bdf.to_string())?;
        let _ = self.inv.register_child(be_register, id).unwrap();

        backend.attach(vioblk.clone(), self.disp)?;
        pci_helper.attach_device(bdf, vioblk)?;
        Ok(())
    }

    pub fn initialize_nvme_block(
        &self,
        pci_helper: &PciHelper,
        bdf: pci::Bdf,
        name: String,
        backend: Arc<dyn block::Backend>,
        be_register: ChildRegister,
    ) -> Result<(), Error> {
        let be_info = backend.info();
        let nvme = nvme::PciNvme::create(name, be_info);
        let id = self.inv.register_instance(&nvme, bdf.to_string())?;
        let _ = self.inv.register_child(be_register, id).unwrap();

        backend.attach(nvme.clone(), self.disp)?;
        pci_helper.attach_device(bdf, nvme)?;
        Ok(())
    }

    pub fn initialize_vnic(
        &self,
        pci_helper: &PciHelper,
        vnic_name: &str,
        bdf: pci::Bdf,
    ) -> Result<(), Error> {
        let hdl = self.machine.get_hdl();
        let viona = virtio::PciVirtioViona::new(vnic_name, 0x100, &hdl)?;
        let _id = self.inv.register_instance(&viona, bdf.to_string())?;
        pci_helper.attach_device(bdf, viona)?;
        Ok(())
    }

    pub fn initialize_crucible(
        &self,
        pci_helper: &PciHelper,
        disk: &propolis_client::api::DiskRequest,
        bdf: pci::Bdf,
    ) -> Result<(), Error> {
        info!(self.log, "Creating Crucible disk from {:#?}", disk);
        let be = propolis::block::CrucibleBackend::create(
            disk.gen,
            disk.volume_construction_request.clone(),
            disk.read_only,
        )?;

        info!(self.log, "Creating ChildRegister");
        let creg = ChildRegister::new(&be, Some(be.get_uuid()?.to_string()));

        match disk.device.as_ref() {
            "virtio" => {
                info!(self.log, "Calling initialize_virtio_block");
                self.initialize_virtio_block(pci_helper, bdf, be, creg)
            }
            "nvme" => {
                info!(self.log, "Calling initialize_nvme_block");
                self.initialize_nvme_block(
                    pci_helper,
                    bdf,
                    disk.name.clone(),
                    be,
                    creg,
                )
            }
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Bad disk device!",
            )),
        }
    }

    pub fn initialize_in_memory_virtio_from_bytes(
        &self,
        pci_helper: &PciHelper,
        name: impl Into<String>,
        bytes: Vec<u8>,
        bdf: pci::Bdf,
        read_only: bool,
    ) -> Result<(), Error> {
        info!(self.log, "Creating in-memory disk from bytes");
        let be =
            propolis::block::InMemoryBackend::create(bytes, read_only, 512)?;

        info!(self.log, "Creating ChildRegister");
        let creg = ChildRegister::new(&be, Some(name.into()));

        info!(self.log, "Calling initialize_virtio_block");
        self.initialize_virtio_block(pci_helper, bdf, be, creg)
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
        for mut vcpu in self.mctx.vcpus() {
            vcpu.set_default_capabs().unwrap();
        }
        Ok(())
    }
}
