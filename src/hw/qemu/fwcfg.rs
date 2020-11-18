use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Weak};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::pio::{PioBus, PioDev};

use byteorder::{ByteOrder, BE, LE};

const FW_CFG_IOP_SELECTOR: u16 = 0x0510;
const FW_CFG_IOP_DATA: u16 = 0x0511;
const FW_CFG_IOP_DMA_HI: u16 = 0x0514;
const FW_CFG_IOP_DMA_LO: u16 = 0x0518;

#[derive(Copy, Clone)]
pub enum LegacyId {
    Signature = 0x0000,
    Id = 0x0001,
    Uuid = 0x0002,
    RamSize = 0x0003,
    GraphicsEna = 0x0004,
    SmpCpuCount = 0x0005,
    MachineId = 0x0006,
    KernelAddr = 0x0007,
    KernelSize = 0x0008,
    KernelCmdline = 0x0009,
    InitrdAddr = 0x000a,
    InitrdSize = 0x000b,
    BootDevice = 0x000c,
    NumaData = 0x000d,
    BootMenu = 0x000e,
    MaxCpuCount = 0x000f,
    KernelEntry = 0x0010,
    KernelData = 0x0011,
    InitrdData = 0x0012,
    CmdlineAddr = 0x0013,
    CmdlineSize = 0x0014,
    CmdlineData = 0x0015,
    KernelSetupAddr = 0x0016,
    KernelSetupSize = 0x0017,
    KernelSetupData = 0x0018,
    FileDir = 0x0019,
}
impl LegacyId {
    fn path(&self) -> &'static str {
        match self {
            LegacyId::Signature => "signature",
            LegacyId::Id => "id",
            LegacyId::Uuid => "uuid",
            LegacyId::RamSize => "ram_size",
            LegacyId::GraphicsEna => "nographic",
            LegacyId::SmpCpuCount => "nb_cpus",
            LegacyId::MachineId => "machine_id",
            LegacyId::KernelAddr => "kernel_addr",
            LegacyId::KernelSize => "kernel_size",
            LegacyId::KernelCmdline => "kernel_cmdline",
            LegacyId::InitrdAddr => "initrd_addr",
            LegacyId::InitrdSize => "initrd_size",
            LegacyId::BootDevice => "boot_device",
            LegacyId::NumaData => "numa",
            LegacyId::BootMenu => "boot_menu",
            LegacyId::MaxCpuCount => "max_cpus",
            LegacyId::KernelEntry => "kernel_entry",
            LegacyId::KernelData => "kernel_data",
            LegacyId::InitrdData => "initrd_data",
            LegacyId::CmdlineAddr => "cmdline_addr",
            LegacyId::CmdlineSize => "cmdline_size",
            LegacyId::CmdlineData => "cmdline_data",
            LegacyId::KernelSetupAddr => "setup_addr",
            LegacyId::KernelSetupSize => "setup_size",
            LegacyId::KernelSetupData => "setup_data",
            LegacyId::FileDir => "file_dir",
        }
    }
}
pub enum LegacyX86Id {
    AcpiTables = 0x8000,
    SmbiosTables = 0x8001,
    Irq0Override = 0x8002,
    E820Table = 0x8003,
    HpetData = 0x8004,
}

const ITEM_INVALID: u16 = 0xffff;
const ITEMS_FILE_START: u16 = 0x0020;
const ITEMS_FILE_END: u16 = 0x1000;
const ITEMS_ARCH_START: u16 = 0x8000;
const ITEMS_ARCH_END: u16 = 0x9000;

const FW_CFG_VER_BASE: u32 = 1 << 0;
const FW_CFG_VER_DMA: u32 = 1 << 1;

pub struct FixedItem {
    data: Vec<u8>,
}
impl FixedItem {
    pub fn new(data: Vec<u8>) -> Box<dyn ItemContent> {
        Box::new(Self { data })
    }
    pub fn new_u32(val: u32) -> Box<dyn ItemContent> {
        let mut buf = [0u8; 4];
        LE::write_u32(&mut buf, val);
        Self::new(buf.to_vec())
    }
}
impl ItemContent for FixedItem {
    fn size(&self) -> u32 {
        self.data.len() as u32
    }
    fn read_byte(&self, offset: u32) -> Option<u8> {
        if offset >= self.data.len() as u32 {
            None
        } else {
            let val = self.data[offset as usize];
            Some(val)
        }
    }
}

pub trait ItemContent: Send + 'static {
    fn size(&self) -> u32;
    fn read_byte(&self, offset: u32) -> Option<u8>;
}

struct Item {
    path: String,
    content: Box<dyn ItemContent>,
}

struct ItemDir {
    items: BTreeMap<u16, Item>,
    path_to_sel: HashMap<String, u16>,
    next_sel: u16,
}
impl ItemDir {
    fn new() -> Self {
        Self {
            items: BTreeMap::new(),
            path_to_sel: HashMap::new(),
            next_sel: ITEMS_FILE_START,
        }
    }
}

#[derive(Default)]
struct PioState {
    selector: u16,
    finished: bool,
    offset: u32,
}

pub struct FwCfg {
    pio_state: Mutex<PioState>,
    dir: Mutex<ItemDir>,
}
impl FwCfg {
    pub fn create(pio: &PioBus) -> Arc<Self> {
        let this = Arc::new(Self {
            pio_state: Mutex::new(PioState::default()),
            dir: Mutex::new(ItemDir::new()),
        });

        this.add_legacy(
            LegacyId::Signature,
            FixedItem::new("QEMU".as_bytes().to_vec()),
        );
        this.add_legacy(LegacyId::Id, FixedItem::new_u32(FW_CFG_VER_BASE));

        this.attach(pio);

        this
    }
    fn attach(self: &Arc<Self>, pio: &PioBus) {
        pio.register(
            FW_CFG_IOP_SELECTOR,
            1,
            Arc::downgrade(self) as Weak<dyn PioDev>,
            0,
        )
        .unwrap();
        pio.register(
            FW_CFG_IOP_DATA,
            1,
            Arc::downgrade(self) as Weak<dyn PioDev>,
            0,
        )
        .unwrap();
    }
    fn read_byte(&self, selector: u16, offset: u32) -> Option<u8> {
        let dir = self.dir.lock().unwrap();
        if let Some(item) = dir.items.get(&selector) {
            item.content.read_byte(offset)
        } else {
            None
        }
    }
    pub fn add_legacy(
        &self,
        sel: LegacyId,
        content: Box<dyn ItemContent>,
    ) -> bool {
        let mut dir = self.dir.lock().unwrap();
        let raw_sel = sel as u16;
        let legacy_path = sel.path().to_string();
        if dir.items.get(&raw_sel).is_none()
            && dir.path_to_sel.get(&legacy_path).is_none()
        {
            let item = Item { path: legacy_path.clone(), content };
            dir.items.insert(raw_sel, item);
            dir.path_to_sel.insert(legacy_path, raw_sel);
            true
        } else {
            false
        }
    }
}

impl PioDev for FwCfg {
    fn pio_rw(&self, port: u16, _ident: usize, rwo: &mut RWOp, _ctx: &DispCtx) {
        match port {
            FW_CFG_IOP_SELECTOR => match rwo {
                RWOp::Read(ro) => {
                    if ro.offset == 0 && ro.buf.len() == 2 {
                        let state = self.pio_state.lock().unwrap();
                        LE::write_u16(&mut ro.buf, state.selector);
                    }
                }
                RWOp::Write(wo) => {
                    if wo.offset == 0 && wo.buf.len() == 2 {
                        let mut state = self.pio_state.lock().unwrap();
                        state.selector = LE::read_u16(wo.buf);
                        state.offset = 0;
                        state.finished = false;
                    }
                }
            },
            FW_CFG_IOP_DATA => {
                match rwo {
                    RWOp::Read(ro) => {
                        let mut state = self.pio_state.lock().unwrap();
                        if ro.buf.len() != 1 || state.finished {
                            for b in ro.buf.iter_mut() {
                                *b = 0
                            }
                            return;
                        }
                        if let Some(val) =
                            self.read_byte(state.selector, state.offset)
                        {
                            state.offset += 1;
                            ro.buf[0] = val;
                        } else {
                            state.finished = true;
                            ro.buf[0] = 0;
                        }
                    }
                    RWOp::Write(wo) => {
                        // XXX: ignore writes to data area
                    }
                }
            }
            FW_CFG_IOP_DMA_HI | FW_CFG_IOP_DMA_LO => {
                // XXX: DMA interface not supported for now
            }
            _ => {
                panic!("unexpected port {:x}", port);
            }
        }
    }
}
