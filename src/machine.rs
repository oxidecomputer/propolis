use std::io::{Read, Result};
use std::sync::{Arc, Mutex};

use crate::devices::rtc::Rtc;
use crate::intr_pins::IsaPIC;
use crate::pci::{PciBus, PORT_PCI_CONFIG_ADDR, PORT_PCI_CONFIG_DATA};
use crate::pio::{PioBus, PioDev};
use crate::util::aspace::ASpace;
use crate::util::regmap::{Flags, RegMap};
use crate::vcpu::VcpuHdl;
use crate::vm::VmmHdl;

// XXX: need some arb limit for now
const MAX_PHYSMEM: usize = 0x1_0000_0000;
const LEGACY_PIC_PINS: u8 = 32;

#[repr(u8)]
enum Memseg {
    LowMem,
    Bootrom,
}

pub struct Machine {
    hdl: Arc<VmmHdl>,
    max_cpu: u8,
    cpus: Mutex<Vec<Option<VcpuHdl>>>,
    state_lock: Mutex<()>,

    map_physmem: Mutex<ASpace<()>>,
    bus_mmio: Mutex<ASpace<()>>,
    bus_pio: PioBus,
    pci_root: Arc<PciBus>,
    isa_pic: Arc<IsaPIC>,
}

impl Machine {
    pub fn new(hdl: VmmHdl, max_cpu: u8) -> Arc<Self> {
        assert!(max_cpu > 0);
        //let bus_pci = Arc::new(PciBus::new());
        let arc_hdl = Arc::new(hdl);

        let mut cpus = Vec::with_capacity(max_cpu as usize);
        for n in 0..max_cpu {
            cpus.push(Some(VcpuHdl::from_vmhdl(arc_hdl.clone(), n as i32)));
        }

        let pic = IsaPIC::new(LEGACY_PIC_PINS, arc_hdl.clone());
        let pci_root = Arc::new(PciBus::new(Arc::downgrade(&pic)));
        Arc::new(Self {
            hdl: arc_hdl,
            max_cpu,
            cpus: Mutex::new(cpus),
            state_lock: Mutex::new(()),

            map_physmem: Mutex::new(ASpace::new(0, MAX_PHYSMEM)),
            bus_mmio: Mutex::new(ASpace::new(0, MAX_PHYSMEM)),
            bus_pio: PioBus::new(),
            pci_root,
            isa_pic: pic,
        })
    }

    pub fn vcpu(&self, id: i32) -> VcpuHdl {
        let mut cpus = self.cpus.lock().unwrap();
        std::mem::replace(&mut cpus[id as usize], None).unwrap()
    }

    pub fn setup_lowmem(&self, size: usize) -> Result<()> {
        let segid = Memseg::LowMem as i32;
        self.hdl.create_memseg(segid, size, None)?;
        let mut pmap = self.map_physmem.lock().unwrap();
        self.hdl.map_memseg(segid, 0, size, 0, bhyve_api::PROT_ALL)?;
        pmap.register(0, size, ()).unwrap();

        Ok(())
    }

    pub fn setup_bootrom(&self, size: usize) -> Result<()> {
        let segid = Memseg::Bootrom as i32;
        // map the bootrom so the first instruction lines up at 0xfffffff0
        let gpa = 0x1_0000_0000 - size;
        self.hdl.create_memseg(segid, size, Some("bootrom"))?;

        let mut pmap = self.map_physmem.lock().unwrap();
        self.hdl.map_memseg(
            segid,
            gpa,
            size,
            0,
            bhyve_api::PROT_READ | bhyve_api::PROT_EXEC,
        )?;
        pmap.register(gpa, size, ()).unwrap();

        Ok(())
    }

    pub fn populate_bootrom<F>(&self, input: &mut F, size: usize) -> Result<()>
    where
        F: Read,
    {
        let (ptr, buf) = unsafe {
            let ptr = self.hdl.mmap_seg(Memseg::Bootrom as i32, size)?;
            assert!(!ptr.is_null());
            let buf = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
            (ptr, buf)
        };

        let lock = self.state_lock.lock().unwrap();
        let res = match input.read(buf) {
            Ok(n) if n == size => Ok(()),
            Ok(_) => {
                // TODO: handle short read
                Ok(())
            }
            Err(e) => Err(e),
        };
        drop(lock);

        unsafe {
            libc::munmap(ptr as *mut libc::c_void, size);
        }

        res
    }

    pub fn initalize_rtc(&self, lowmem: usize) -> Result<()> {
        let lock = self.state_lock.lock().unwrap();
        Rtc::set_time(&self.hdl)?;
        Rtc::store_memory_sizing(&self.hdl, lowmem, None)?;
        drop(lock);
        Ok(())
    }

    pub fn wire_pci_root(&self) {
        self.bus_pio.register(
            PORT_PCI_CONFIG_ADDR,
            4,
            &(self.pci_root.clone() as Arc<dyn PioDev>),
        );
        self.bus_pio.register(
            PORT_PCI_CONFIG_DATA,
            4,
            &(self.pci_root.clone() as Arc<dyn PioDev>),
        );
    }

    pub fn setup_legacy_interrupts(&self) {
        // TODO: certain interrupts expect to be level-triggered
    }
}

#[derive(Clone)]
pub struct MachineCtx {
    vm: Arc<Machine>,
}

impl MachineCtx {
    pub fn new(vm: &Arc<Machine>) -> Self {
        Self { vm: vm.clone() }
    }

    pub fn with_pio<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&PioBus) -> R,
    {
        f(&self.vm.bus_pio)
    }
    pub fn with_pci<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&PciBus) -> R,
    {
        f(&self.vm.pci_root)
    }
}
