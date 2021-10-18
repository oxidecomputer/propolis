use anyhow::Result;
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::num::NonZeroUsize;
use std::sync::Arc;
use tokio::runtime::Handle;

use propolis::block;
use propolis::chardev::{self, BlockingSource, Source};
use propolis::common::PAGE_SIZE;
use propolis::dispatch::Dispatcher;
use propolis::hw::chipset::{i440fx::I440Fx, Chipset};
use propolis::hw::ibmpc;
use propolis::hw::pci;
use propolis::hw::ps2ctrl::PS2Ctrl;
use propolis::hw::qemu::{debug::QemuDebugPort, fwcfg, ramfb};
use propolis::hw::uart::LpcUart;
use propolis::hw::virtio;
use propolis::instance::Instance;
use propolis::inventory::{ChildRegister, EntityID, Inventory};
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

    // Allow propolis to use the existing tokio runtime for spawning and
    // dispatching its tasks
    let rt_handle = Some(Handle::current());
    let inst = Instance::create(builder.finalize()?, rt_handle)?;
    inst.spawn_vcpu_workers(propolis::vcpu_run_loop)?;
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
            .register(&chipset, "chipset".to_string(), None)
            .map_err(|e| -> std::io::Error { e.into() })?;
        Ok(RegisteredChipset(chipset, id))
    }

    pub fn initialize_uart(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<Serial<LpcUart>, Error> {
        let cid = Some(chipset.id());
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
            self.inv
                .register(&dev, name.to_string(), cid)
                .map_err(|e| -> std::io::Error { e.into() })?;
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
        self.inv
            .register(&ps2_ctrl, "ps2_ctrl".to_string(), Some(chipset.id()))
            .map_err(|e| -> std::io::Error { e.into() })?;
        Ok(())
    }

    pub fn initialize_qemu_debug_port(&self) -> Result<(), Error> {
        let dbg = QemuDebugPort::create(self.mctx.pio());
        let debug_file = std::fs::File::create("debug.out")?;
        let poller = chardev::BlockingFileOutput::new(debug_file)?;
        poller.attach(Arc::clone(&dbg) as Arc<dyn BlockingSource>, self.disp);
        self.inv
            .register(&dbg, "debug".to_string(), None)
            .map_err(|e| -> std::io::Error { e.into() })?;
        Ok(())
    }

    pub fn initialize_virtio_block(
        &self,
        chipset: &RegisteredChipset,
        bdf: pci::Bdf,
        backend: Arc<dyn block::Backend>,
        be_register: ChildRegister,
    ) -> Result<(), Error> {
        let be_info = backend.info();
        let vioblk = virtio::VirtioBlock::create(0x100, be_info);
        let id = self
            .inv
            .register(&vioblk, format!("vioblk-{}", bdf), None)
            .map_err(|e| -> std::io::Error { e.into() })?;
        let _ = self.inv.register_child(be_register, id).unwrap();

        let blk = vioblk
            .inner_dev::<virtio::pci::PciVirtio>()
            .inner_dev::<virtio::block::VirtioBlock>();
        backend.attach(blk, self.disp);
        chipset.device().pci_attach(bdf, vioblk);

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
        let _id = self
            .inv
            .register(&viona, format!("viona-{}", bdf), None)
            .map_err(|e| -> std::io::Error { e.into() })?;
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
            .register(&fwcfg_dev, "fwcfg".to_string(), Some(chipset.id()))
            .map_err(|e| -> std::io::Error { e.into() })?;
        self.inv
            .register(&ramfb, "ramfb".to_string(), Some(chipset.id()))
            .map_err(|e| -> std::io::Error { e.into() })?;
        Ok(())
    }

    pub fn initialize_cpus(&self) -> Result<(), Error> {
        for mut vcpu in self.mctx.vcpus() {
            vcpu.set_default_capabs().unwrap();
        }
        Ok(())
    }
}
