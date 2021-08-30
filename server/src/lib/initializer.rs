use anyhow::Result;
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::sync::Arc;

use propolis::bhyve_api;
use propolis::block;
use propolis::block::BlockDev;
use propolis::chardev::Source;
use propolis::common::PAGE_SIZE;
use propolis::dispatch::{DispCtx, Dispatcher};
use propolis::hw::chipset::{i440fx::I440Fx, Chipset};
use propolis::hw::ibmpc;
use propolis::hw::pci;
use propolis::hw::ps2ctrl::PS2Ctrl;
use propolis::hw::qemu::{debug::QemuDebugPort, fwcfg, ramfb};
use propolis::hw::uart::LpcUart;
use propolis::hw::virtio;
use propolis::instance::Instance;
use propolis::inventory::{EntityID, Inventory};
use propolis::vmm::{self, Builder, Machine, MachineCtx, Prot};

use crate::serial::Serial;

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
    let inst = Instance::create(builder, propolis::vcpu_run_loop)?;
    Ok(inst)
}

pub struct RegisteredChipset(Arc<I440Fx>, EntityID);
impl RegisteredChipset {
    pub fn device(&self) -> &Arc<I440Fx> {
        &self.0
    }
    pub fn id(&self) -> EntityID {
        self.1
    }
}

pub struct MachineInitializer<'a> {
    machine: &'a Machine,
    mctx: &'a MachineCtx,
    disp: &'a Dispatcher,
    inv: &'a Inventory,
}

impl<'a> MachineInitializer<'a> {
    pub fn new(
        machine: &'a Machine,
        mctx: &'a MachineCtx,
        disp: &'a Dispatcher,
        inv: &'a Inventory,
    ) -> Self {
        MachineInitializer { machine, mctx, disp, inv }
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

    pub fn initialize_chipset(&self) -> Result<RegisteredChipset, Error> {
        let hdl = self.machine.get_hdl();
        let chipset = I440Fx::create(Arc::clone(&hdl));
        chipset.attach(self.mctx);
        let id = self
            .inv
            .register_root(chipset.clone(), "chipset".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        Ok(RegisteredChipset(chipset, id))
    }

    pub fn initialize_uart(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<Serial<DispCtx, LpcUart>, Error> {
        // UARTs
        let com1 =
            LpcUart::new(chipset.device().irq_pin(ibmpc::IRQ_COM1).unwrap());
        let com2 =
            LpcUart::new(chipset.device().irq_pin(ibmpc::IRQ_COM2).unwrap());
        let com3 =
            LpcUart::new(chipset.device().irq_pin(ibmpc::IRQ_COM3).unwrap());
        let com4 =
            LpcUart::new(chipset.device().irq_pin(ibmpc::IRQ_COM4).unwrap());

        com1.source_set_autodiscard(true);
        com2.source_set_autodiscard(true);
        com3.source_set_autodiscard(true);
        com4.source_set_autodiscard(true);

        let pio = self.mctx.pio();
        LpcUart::attach(&com1, pio, ibmpc::PORT_COM1);
        LpcUart::attach(&com2, pio, ibmpc::PORT_COM2);
        LpcUart::attach(&com3, pio, ibmpc::PORT_COM3);
        LpcUart::attach(&com4, pio, ibmpc::PORT_COM4);
        self.inv
            .register(chipset.id(), com1.clone(), "com1".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        self.inv
            .register(chipset.id(), com2, "com2".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        self.inv
            .register(chipset.id(), com3, "com3".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        self.inv
            .register(chipset.id(), com4, "com4".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;

        let sink_size = 15;
        let source_size = 4095;
        Ok(Serial::new(com1, sink_size, source_size))
    }

    pub fn initialize_ps2(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<(), Error> {
        let pio = self.mctx.pio();
        let ps2_ctrl = PS2Ctrl::create();
        ps2_ctrl.attach(pio, chipset.device().as_ref());
        self.inv
            .register(chipset.id(), ps2_ctrl, "ps2_ctrl".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        Ok(())
    }

    pub fn initialize_qemu_debug_port(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<(), Error> {
        let debug = std::fs::File::create("debug.out").unwrap();
        let buffered = std::io::LineWriter::new(debug);
        let pio = self.mctx.pio();
        let dbg = QemuDebugPort::create(
            Some(Box::new(buffered) as Box<dyn std::io::Write + Send>),
            pio,
        );
        self.inv
            .register(chipset.id(), dbg, "debug".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        Ok(())
    }

    pub fn initialize_block<P: AsRef<std::path::Path>>(
        &self,
        chipset: &RegisteredChipset,
        path: P,
        bdf: pci::Bdf,
        readonly: bool,
    ) -> Result<(), Error> {
        let plain = block::FileBdev::create(path.as_ref(), readonly)?;

        let vioblk = virtio::VirtioBlock::create(
            0x100,
            Arc::clone(&plain)
                as Arc<dyn block::BlockDev<virtio::block::Request>>,
        );
        chipset.device().pci_attach(bdf, vioblk);

        plain.start_dispatch(
            format!("bdev-{} thread", path.as_ref().to_string_lossy()),
            &self.disp,
        );
        Ok(())
    }

    pub fn initialize_vnic(
        &self,
        chipset: &RegisteredChipset,
        vnic_name: &str,
        bdf: pci::Bdf,
    ) -> Result<(), Error> {
        let hdl = self.machine.get_hdl();
        let viona = virtio::viona::VirtioViona::create(vnic_name, 0x100, &hdl)?;
        chipset.device().pci_attach(bdf, viona);
        Ok(())
    }

    pub fn initialize_fwcfg(
        &self,
        chipset: &RegisteredChipset,
        cpus: u8,
    ) -> Result<(), Error> {
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

        self.inv
            .register(chipset.id(), fwcfg_dev, "fwcfg".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        self.inv
            .register(chipset.id(), ramfb, "ramfb".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        Ok(())
    }

    pub fn initialize_cpus(&self) -> Result<(), Error> {
        let ncpu = self.mctx.max_cpus();
        for id in 0..ncpu {
            let mut vcpu = self.machine.vcpu(id);
            vcpu.set_default_capabs().unwrap();
            vcpu.reboot_state().unwrap();
            vcpu.activate().unwrap();
            // Set BSP to start up
            if id == 0 {
                vcpu.set_run_state(bhyve_api::VRS_RUN).unwrap();
                vcpu.set_reg(bhyve_api::vm_reg_name::VM_REG_GUEST_RIP, 0xfff0)
                    .unwrap();
            }
        }
        Ok(())
    }
}
