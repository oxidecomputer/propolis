use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::pio::{PioBus, PioDev};
use bits::*;

use byteorder::{ByteOrder, BE, LE};

type Result = std::result::Result<(), &'static str>;

#[allow(unused)]
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
    fn name(&self) -> &'static str {
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

pub trait Item: Send + Sync + 'static {
    #![allow(unused)]
    fn read(&self, ro: &mut ReadOp) -> Result {
        Err("unimplemented")
    }
    fn write(&self, wo: &WriteOp) -> Result {
        Err("unimplemented")
    }
    fn size(&self) -> u32;
}

pub struct FixedItem {
    data: Vec<u8>,
}
impl FixedItem {
    pub fn new_raw(data: Vec<u8>) -> Box<dyn Item> {
        assert!(data.len() <= u32::MAX as usize);
        Box::new(Self { data })
    }
    pub fn new_u32(val: u32) -> Box<dyn Item> {
        let mut buf = [0u8; 4];
        LE::write_u32(&mut buf, val);
        Self::new_raw(buf.to_vec())
    }
}
impl Item for FixedItem {
    fn read(&self, ro: &mut ReadOp) -> Result {
        let off = ro.offset;
        if off + ro.buf.len() > self.data.len() {
            Err("read beyond bounds")
        } else {
            let copy_len = usize::min(self.data.len() - off, ro.buf.len());
            ro.buf[..copy_len]
                .copy_from_slice(&self.data[off..(off + copy_len)]);
            Ok(())
        }
    }
    fn size(&self) -> u32 {
        self.data.len() as u32
    }
}

struct PlaceholderItem {}
impl Item for PlaceholderItem {
    fn read(&self, _ro: &mut ReadOp) -> Result {
        panic!("should never be accessed");
    }
    fn write(&self, _wo: &WriteOp) -> Result {
        panic!("should never be accessed");
    }
    fn size(&self) -> u32 {
        panic!("should never be accessed");
    }
}

const FWCFG_FILENAME_LEN: usize = 56;
#[repr(packed)]
struct FwCfgFileEntry {
    size: u32,
    selector: u16,
    reserved: u16,
    name: [u8; FWCFG_FILENAME_LEN],
}
impl FwCfgFileEntry {
    fn new(selector: u16, name: &str, size: u32) -> Self {
        assert!(name.len() < FWCFG_FILENAME_LEN);

        let mut this = Self {
            size: size.to_be(),
            selector: selector.to_be(),
            reserved: 0,
            name: [0; FWCFG_FILENAME_LEN],
        };
        this.name[..name.len()].copy_from_slice(name.as_ref());
        this
    }
    fn bytes<'a>(&'a self) -> &'a [u8] {
        // SAFETY:  This struct is packed, so it does not have any padding with
        // undefined contents, making the conversion to a byte-slice safe.
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }
    const fn sizeof() -> usize {
        std::mem::size_of::<Self>()
    }
}

#[derive(Default)]
#[repr(C)]
struct FwCfgDmaReq {
    ctrl: u32,
    len: u32,
    addr: u64,
}
impl FwCfgDmaReq {
    fn from_bytes(data: &[u8]) -> Self {
        assert_eq!(data.len(), Self::sizeof());
        Self {
            ctrl: BE::read_u32(&data[0..4]),
            len: BE::read_u32(&data[4..8]),
            addr: BE::read_u64(&data[8..16]),
        }
    }
    const fn sizeof() -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn file_struct_sizing() {
        assert_eq!(std::mem::size_of::<FwCfgFileEntry>(), 64);
        assert_eq!(std::mem::size_of::<FwCfgDmaReq>(), 16);
    }
}

struct Entry {
    content: Box<dyn Item>,
}

struct ItemDir {
    entries: BTreeMap<u16, Entry>,
    sorted_names: Vec<(String, u16)>,
}
impl ItemDir {
    fn file_entry(&self, idx: usize) -> FwCfgFileEntry {
        let (name, selector) = &self.sorted_names[idx];

        let size = if *selector == LegacyId::FileDir as u16 {
            self.sorted_names.len() as u32 * FwCfgFileEntry::sizeof() as u32
        } else {
            self.entries.get(&selector).unwrap().content.size()
        };
        FwCfgFileEntry::new(*selector, &name, size)
    }
    fn read(&self, mut ro: ReadOp) -> Result {
        let ent_size = FwCfgFileEntry::sizeof();
        let dir_limit = self.sorted_names.len() * ent_size;
        let buf_limit = ro.limit().ok_or("offset overflow")?;
        if ro.offset >= dir_limit || buf_limit > dir_limit {
            return Err("read beyond end");
        }

        while ro.buf.len() > 0 {
            let idx = ro.offset / ent_size;
            let off = ro.offset % ent_size;
            let entry = self.file_entry(idx);
            let entry_buf = entry.bytes();
            let copy_len = usize::min(entry_buf.len() - off, ro.buf.len());

            ro.buf[..copy_len]
                .copy_from_slice(&entry_buf[off..(off + copy_len)]);
            ro = ro.consume(copy_len);
        }
        Ok(())
    }
    fn size(&self) -> u32 {
        (self.sorted_names.len() * FwCfgFileEntry::sizeof()) as u32
    }
    fn is_present(&self, selector: u16) -> bool {
        if selector == LegacyId::FileDir as u16 {
            true
        } else {
            self.entries.get(&selector).is_some()
        }
    }
}

pub struct FwCfgBuilder {
    entries: BTreeMap<u16, Entry>,
    name_to_sel: BTreeMap<String, u16>,
    next_sel: u16,
}

impl FwCfgBuilder {
    pub fn new() -> Self {
        let mut this = Self {
            entries: BTreeMap::new(),
            name_to_sel: BTreeMap::new(),
            next_sel: ITEMS_FILE_START,
        };
        this.add_legacy(
            LegacyId::Signature,
            FixedItem::new_raw(b"QEMU".to_vec()),
        )
        .unwrap();
        this.add_legacy(
            LegacyId::Id,
            FixedItem::new_u32(FW_CFG_VER_BASE | FW_CFG_VER_DMA),
        )
        .unwrap();

        // Occupy the filedir slot with a placeholder so it cannot be overriden
        // by an outside consumer.  Since it requires special care, requests to
        // that endpoint will be dispatched "manually".
        this.add_impl(
            LegacyId::FileDir as u16,
            LegacyId::FileDir.name().to_string(),
            Box::new(PlaceholderItem {}),
        )
        .unwrap();

        this
    }

    pub fn add_legacy(
        &mut self,
        sel: LegacyId,
        content: Box<dyn Item>,
    ) -> Result {
        self.add_impl(sel as u16, sel.name().to_string(), content)
    }

    fn add_impl(
        &mut self,
        sel: u16,
        name: String,
        content: Box<dyn Item>,
    ) -> Result {
        if self.entries.get(&sel).is_none()
            && self.name_to_sel.get(&name).is_none()
        {
            let item = Entry { content };
            self.entries.insert(sel, item);
            self.name_to_sel.insert(name, sel);
            Ok(())
        } else {
            Err("entry already exists")
        }
    }

    pub fn finalize(self) -> Arc<FwCfg> {
        let mut sorted_names: Vec<(String, u16)> =
            self.name_to_sel.into_iter().collect();
        // Should be sorted coming out of the btree, but be extra sure.
        sorted_names.sort();
        let dir = ItemDir { entries: self.entries, sorted_names };
        let this = Arc::new(FwCfg::new(dir));

        this
    }
}

#[derive(Default)]
struct AccessState {
    addr_high: u32,
    addr_low: u32,
    selector: u16,
    offset: u32,
}

pub struct FwCfg {
    dir: ItemDir,
    state: Mutex<AccessState>,
}
impl FwCfg {
    fn new(dir: ItemDir) -> Self {
        Self { dir, state: Mutex::new(Default::default()) }
    }

    pub fn attach(self: &Arc<Self>, pio: &PioBus) {
        let ports = [
            (FW_CFG_IOP_SELECTOR, 1),
            (FW_CFG_IOP_DATA, 1),
            (FW_CFG_IOP_DMA_HI, 4),
            (FW_CFG_IOP_DMA_LO, 4),
        ];
        for (port, len) in ports.iter() {
            pio.register(
                *port,
                *len,
                Arc::downgrade(self) as Weak<dyn PioDev>,
                0,
            )
            .unwrap()
        }
    }

    fn read(&self, selector: u16, ro: &mut ReadOp) -> Result {
        if selector == LegacyId::FileDir as u16 {
            self.dir.read(ReadOp::new(ro.offset, &mut ro.buf))
        } else {
            if let Some(item) = self.dir.entries.get(&selector) {
                item.content.read(ro)
            } else {
                Err("entry not found")
            }
        }
    }

    fn size(&self, selector: u16) -> u32 {
        if selector == LegacyId::FileDir as u16 {
            self.dir.size()
        } else {
            if let Some(item) = self.dir.entries.get(&selector) {
                item.content.size()
            } else {
                0
            }
        }
    }

    fn dma_initiate(
        &self,
        mut state: MutexGuard<AccessState>,
        ctx: &DispCtx,
    ) -> Result {
        let req_addr = (state.addr_high as u64) << 32 | state.addr_low as u64;
        // initiating a DMA transfer clears the addr contents
        state.addr_high = 0;
        state.addr_low = 0;

        let mut desc_buf = [0u8; FwCfgDmaReq::sizeof()];
        let mem = ctx.mctx.memctx();
        mem.read_into(
            GuestAddr(req_addr),
            &mut desc_buf,
            FwCfgDmaReq::sizeof(),
        )
        .ok_or("bad GPA")?;

        let dma_req = FwCfgDmaReq::from_bytes(&desc_buf);

        let res = self.dma_operation(state, &dma_req, ctx);

        if !mem.write(
            GuestAddr(req_addr),
            &match res {
                Ok(_) => 0,
                Err(_) => u32::to_be(FwCfgDmaCtrl::ERROR.bits()),
            },
        ) {
            return Err("bad GPA");
        }
        res
    }
    fn dma_operation(
        &self,
        mut state: MutexGuard<AccessState>,
        dma_req: &FwCfgDmaReq,
        ctx: &DispCtx,
    ) -> Result {
        let mut ctrl = FwCfgDmaCtrl::from_bits_truncate(dma_req.ctrl);
        if ctrl.contains(FwCfgDmaCtrl::ERROR) {
            return Err("request already in error");
        }

        if ctrl.contains(FwCfgDmaCtrl::SELECT) {
            let selector = (dma_req.ctrl >> 16) as u16;
            state.selector = selector;
            state.offset = 0;
            if !self.dir.is_present(selector) {
                return Err("selector not found");
            }
            ctrl.remove(FwCfgDmaCtrl::SELECT);
        }

        let end_offset = if ctrl.intersects(
            FwCfgDmaCtrl::READ | FwCfgDmaCtrl::WRITE | FwCfgDmaCtrl::SKIP,
        ) {
            state.offset.checked_add(dma_req.len).ok_or("offset overflow")?
        } else {
            0
        };

        // match the same command precedence as qemu (read, write, skip)
        if ctrl.contains(FwCfgDmaCtrl::READ) {
            let res = self.dma_read(
                state.selector,
                dma_req.addr,
                state.offset,
                dma_req.len,
                ctx,
            );
            if res.is_err() {
                return res;
            }

            state.offset = end_offset;
        } else if ctrl.contains(FwCfgDmaCtrl::WRITE) {
            return Err("XXX writes disallowed for now");
        } else if ctrl.contains(FwCfgDmaCtrl::SKIP) {
            state.offset = end_offset;
        }
        Ok(())
    }
    fn dma_read(
        &self,
        selector: u16,
        mut addr: u64,
        mut offset: u32,
        mut len: u32,
        ctx: &DispCtx,
    ) -> Result {
        const BUF_LEN: u32 = 64;
        let mut buf = [0u8; BUF_LEN as usize];
        let mem = ctx.mctx.memctx();
        let mut valid_remain = self.size(selector).saturating_sub(offset);

        while len > 0 {
            if valid_remain > 0 {
                let copy_len = len.min(BUF_LEN).min(valid_remain) as usize;
                let mut ro = ReadOp::new(offset as usize, &mut buf[..copy_len]);
                self.read(selector, &mut ro)?;
                mem.write_from(GuestAddr(addr), &buf[..copy_len], copy_len)
                    .ok_or("bad GPA")?;

                len -= copy_len as u32;
                valid_remain -= copy_len as u32;
                offset += copy_len as u32;
                addr += copy_len as u64;
            } else {
                // write zeroes for everything past the end of the entry
                mem.write_bytes(GuestAddr(addr), 0, len as usize);
                return Ok(());
            }
        }
        Ok(())
    }
}

impl PioDev for FwCfg {
    fn pio_rw(&self, port: u16, _ident: usize, rwo: &mut RWOp, ctx: &DispCtx) {
        let mut state = self.state.lock().unwrap();
        match port {
            FW_CFG_IOP_SELECTOR => match rwo {
                RWOp::Read(ro) => match ro.buf.len() {
                    2 => LE::write_u16(&mut ro.buf, state.selector),
                    1 => ro.buf[0] = state.selector as u8,
                    _ => {}
                },
                RWOp::Write(wo) => {
                    match wo.buf.len() {
                        2 => state.selector = LE::read_u16(wo.buf),
                        1 => state.selector = wo.buf[0] as u16,
                        _ => {}
                    }
                    state.offset = 0;
                }
            },
            FW_CFG_IOP_DATA => {
                match rwo {
                    RWOp::Read(ro) => {
                        if ro.buf.len() != 1 {
                            ro.fill(0);
                            return;
                        }

                        let res = self.read(
                            state.selector,
                            &mut ReadOp::new(
                                state.offset as usize,
                                &mut ro.buf[..1],
                            ),
                        );
                        if res.is_err() {
                            ro.buf[0] = 0;
                        }
                        state.offset = state.offset.saturating_add(1);
                    }
                    RWOp::Write(_wo) => {
                        // XXX: ignore writes to data area
                    }
                }
            }
            FW_CFG_IOP_DMA_HI => match rwo {
                RWOp::Read(ro) => {
                    if ro.buf.len() != 4 {
                        ro.fill(0);
                    } else {
                        BE::write_u32(&mut ro.buf, state.addr_high);
                    }
                }
                RWOp::Write(wo) => {
                    if wo.buf.len() == 4 {
                        state.addr_high = BE::read_u32(&wo.buf);
                    }
                }
            },
            FW_CFG_IOP_DMA_LO => match rwo {
                RWOp::Read(ro) => {
                    if ro.buf.len() != 4 {
                        ro.fill(0);
                    } else {
                        BE::write_u32(&mut ro.buf, state.addr_low);
                    }
                }
                RWOp::Write(wo) => {
                    if wo.buf.len() == 4 {
                        state.addr_low = BE::read_u32(&wo.buf);
                        let _ = self.dma_initiate(state, ctx);
                    }
                }
            },
            _ => {
                panic!("unexpected port {:x}", port);
            }
        }
    }
}

mod bits {
    #![allow(unused)]

    pub const FW_CFG_IOP_SELECTOR: u16 = 0x0510;
    pub const FW_CFG_IOP_DATA: u16 = 0x0511;
    pub const FW_CFG_IOP_DMA_HI: u16 = 0x0514;
    pub const FW_CFG_IOP_DMA_LO: u16 = 0x0518;

    pub const ITEM_INVALID: u16 = 0xffff;
    pub const ITEMS_FILE_START: u16 = 0x0020;
    pub const ITEMS_FILE_END: u16 = 0x1000;
    pub const ITEMS_ARCH_START: u16 = 0x8000;
    pub const ITEMS_ARCH_END: u16 = 0x9000;

    pub const FW_CFG_VER_BASE: u32 = 1 << 0;
    pub const FW_CFG_VER_DMA: u32 = 1 << 1;

    bitflags! {
        pub struct FwCfgDmaCtrl: u32 {
            const ERROR = 1 << 0;
            const READ = 1 << 1;
            const SKIP = 1 << 2;
            const SELECT = 1 << 3;
            const WRITE = 1 << 4;
        }
    }
}
