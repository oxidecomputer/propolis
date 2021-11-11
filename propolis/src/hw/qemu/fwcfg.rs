use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::migrate::Migrate;
use crate::pio::{PioBus, PioFn};
use bits::*;

use byteorder::{ByteOrder, BE, LE};
use erased_serde::Serialize;

pub type Result = std::result::Result<(), &'static str>;

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
    fn fwcfg_rw(&self, rwo: RWOp, ctx: &DispCtx) -> Result;
    fn size(&self) -> u32;
}

pub struct FixedItem {
    data: Vec<u8>,
}
impl FixedItem {
    pub fn new_raw(data: Vec<u8>) -> Arc<dyn Item> {
        assert!(data.len() <= u32::MAX as usize);
        Arc::new(Self { data })
    }
    pub fn new_u32(val: u32) -> Arc<dyn Item> {
        let mut buf = [0u8; 4];
        LE::write_u32(&mut buf, val);
        Self::new_raw(buf.to_vec())
    }
}
impl Item for FixedItem {
    fn size(&self) -> u32 {
        self.data.len() as u32
    }

    fn fwcfg_rw(&self, rwo: RWOp, _ctx: &DispCtx) -> Result {
        match rwo {
            RWOp::Read(ro) => {
                let off = ro.offset();
                if off + ro.len() > self.data.len() {
                    Err("read beyond bounds")
                } else {
                    let copy_len = usize::min(self.data.len() - off, ro.len());
                    ro.write_bytes(&self.data[off..(off + copy_len)]);
                    Ok(())
                }
            }
            RWOp::Write(_) => Err("writes to fixed items not allowed"),
        }
    }
}

struct PlaceholderItem {}
impl Item for PlaceholderItem {
    fn fwcfg_rw(&self, _rwo: RWOp, _ctx: &DispCtx) -> Result {
        panic!("should never be accessed");
    }
    fn size(&self) -> u32 {
        panic!("should never be accessed");
    }
}

#[allow(unused)]
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
    fn bytes(&self) -> &[u8] {
        // Safety:  This struct is packed, so it does not have any padding with
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
    content: Arc<dyn Item>,
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
    fn read_header(&self, mut ro: ReadOp) -> usize {
        let num = u32::to_be_bytes(self.sorted_names.len() as u32);
        let to_write = &num[ro.offset()..];
        ro.write_bytes(to_write);
        to_write.len()
    }
    fn read_entries(&self, mut ro: ReadOp) -> Result {
        let ent_size = FwCfgFileEntry::sizeof();
        let dir_limit = self.sorted_names.len() * ent_size;
        let offset = ro.offset();
        let buf_limit =
            offset.checked_add(ro.len()).ok_or("offset overflow")?;
        if offset >= dir_limit || buf_limit > dir_limit {
            return Err("read beyond end");
        }
        while ro.avail() > 0 {
            let idx = offset / ent_size;
            let entry_off = offset % ent_size;
            let entry = self.file_entry(idx);
            let entry_buf = entry.bytes();
            let copy_len = usize::min(entry_buf.len() - entry_off, ro.avail());
            ro.write_bytes(&entry_buf[entry_off..(entry_off + copy_len)]);
        }
        Ok(())
    }
    fn read(&self, ro: &mut ReadOp) -> Result {
        let off = ro.offset();
        let header_xfer = if off < 4 {
            self.read_header(ReadOp::new_child(off, ro, off..4))
        } else {
            0
        };
        if off + ro.len() > 4 {
            return self.read_entries(ReadOp::new_child(
                ro.offset() + header_xfer - 4,
                ro,
                header_xfer..,
            ));
        }

        Ok(())
    }
    fn size(&self) -> u32 {
        // header (32-bit count) + N 64-byte entries
        4 + (self.sorted_names.len() * FwCfgFileEntry::sizeof()) as u32
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
            Arc::new(PlaceholderItem {}),
        )
        .unwrap();

        this
    }

    pub fn add_legacy(
        &mut self,
        sel: LegacyId,
        content: Arc<dyn Item>,
    ) -> Result {
        self.add_impl(sel as u16, sel.name().to_string(), content)
    }
    pub fn add_named(&mut self, name: &str, content: Arc<dyn Item>) -> Result {
        let sel = self.next_sel;
        let next_sel = self.next_sel.checked_add(1).ok_or("item overflow")?;
        self.add_impl(sel, name.to_string(), content)?;
        self.next_sel = next_sel;
        Ok(())
    }

    fn add_impl(
        &mut self,
        sel: u16,
        name: String,
        content: Arc<dyn Item>,
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

        Arc::new(FwCfg::new(dir))
    }
}

#[derive(Default)]
struct AccessState {
    addr_high: u32,
    addr_low: u32,
    selector: u16,
    offset: u32,
}
impl AccessState {
    fn dma_addr(&self) -> u64 {
        (self.addr_high as u64) << 32 | self.addr_low as u64
    }
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
        let this = self.clone();
        let piofn = Arc::new(move |port: u16, rwo: RWOp, ctx: &DispCtx| {
            this.pio_rw(port, rwo, ctx)
        }) as Arc<PioFn>;
        for (port, len) in ports.iter() {
            pio.register(*port, *len, piofn.clone()).unwrap()
        }
    }

    fn pio_rw(&self, port: u16, rwo: RWOp, ctx: &DispCtx) {
        let mut state = self.state.lock().unwrap();
        match port {
            FW_CFG_IOP_SELECTOR => match rwo {
                RWOp::Read(ro) => match ro.len() {
                    2 => ro.write_u16(state.selector),
                    1 => ro.write_u8(state.selector as u8),
                    _ => {}
                },
                RWOp::Write(wo) => {
                    match wo.len() {
                        2 => state.selector = wo.read_u16(),
                        1 => state.selector = wo.read_u8() as u16,
                        _ => {}
                    }
                    state.offset = 0;
                }
            },
            FW_CFG_IOP_DATA => {
                match rwo {
                    RWOp::Read(mut ro) => {
                        if ro.len() != 1 {
                            ro.fill(0);
                            return;
                        }

                        let res = self.xfer(
                            state.selector,
                            RWOp::Read(&mut ReadOp::new_child(
                                state.offset as usize,
                                &mut ro,
                                0..1,
                            )),
                            ctx,
                        );
                        if res.is_err() {
                            ro.write_u8(0);
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
                    if ro.len() != 4 {
                        ro.fill(0);
                    } else {
                        ro.write_u32(state.addr_high.to_be());
                    }
                }
                RWOp::Write(wo) => {
                    if wo.len() == 4 {
                        state.addr_high = u32::from_be(wo.read_u32());
                    }
                }
            },
            FW_CFG_IOP_DMA_LO => match rwo {
                RWOp::Read(ro) => {
                    if ro.len() != 4 {
                        ro.fill(0);
                    } else {
                        ro.write_u32(state.addr_low.to_be());
                    }
                }
                RWOp::Write(wo) => {
                    if wo.len() == 4 {
                        state.addr_low = u32::from_be(wo.read_u32());
                        let _ = self.dma_initiate(state, ctx);
                    }
                }
            },
            _ => {
                panic!("unexpected port {:x}", port);
            }
        }
    }

    fn xfer(&self, selector: u16, rwo: RWOp, ctx: &DispCtx) -> Result {
        if selector == LegacyId::FileDir as u16 {
            if let RWOp::Read(ro) = rwo {
                self.dir.read(ro)
            } else {
                Err("filedir not writable")
            }
        } else if let Some(item) = self.dir.entries.get(&selector) {
            item.content.fwcfg_rw(rwo, ctx)
        } else {
            Err("entry not found")
        }
    }

    fn size(&self, selector: u16) -> u32 {
        if selector == LegacyId::FileDir as u16 {
            self.dir.size()
        } else if let Some(item) = self.dir.entries.get(&selector) {
            item.content.size()
        } else {
            0
        }
    }

    fn dma_initiate(
        &self,
        mut state: MutexGuard<AccessState>,
        ctx: &DispCtx,
    ) -> Result {
        let req_addr = state.dma_addr();
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
            state.offset
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
        } else if ctrl.contains(FwCfgDmaCtrl::WRITE) {
            let res = self.dma_write(
                state.selector,
                dma_req.addr,
                state.offset,
                dma_req.len,
                ctx,
            );
            if res.is_err() {
                return res;
            }
        }
        state.offset = end_offset;
        Ok(())
    }
    fn dma_read(
        &self,
        selector: u16,
        addr: u64,
        offset: u32,
        len: u32,
        ctx: &DispCtx,
    ) -> Result {
        let mem = ctx.mctx.memctx();
        let valid_remain = self.size(selector).saturating_sub(offset);

        let written = if valid_remain > 0 {
            let to_write = len.min(valid_remain);
            let mapping = mem
                .writable_region(&GuestRegion(
                    GuestAddr(addr),
                    to_write as usize,
                ))
                .ok_or("bad GPA")?;

            let mut ro = ReadOp::from_mapping(offset as usize, mapping);
            self.xfer(selector, RWOp::Read(&mut ro), ctx)?;
            to_write
        } else {
            0
        };

        // write zeroes for everything past the end of the data
        if written < len {
            mem.write_byte(
                GuestAddr(addr + written as u64),
                0,
                (len - written) as usize,
            );
        }
        Ok(())
    }
    fn dma_write(
        &self,
        selector: u16,
        addr: u64,
        offset: u32,
        len: u32,
        ctx: &DispCtx,
    ) -> Result {
        let mem = ctx.mctx.memctx();
        let valid_remain = self.size(selector).saturating_sub(offset);

        if valid_remain > 0 {
            let to_read = len.min(valid_remain);
            let mapping = mem
                .readable_region(&GuestRegion(
                    GuestAddr(addr),
                    to_read as usize,
                ))
                .ok_or("bad GPA")?;
            let mut wo = WriteOp::from_mapping(offset as usize, mapping);
            self.xfer(selector, RWOp::Write(&mut wo), ctx)?;
        }
        Ok(())
    }
}

impl Entity for FwCfg {
    fn type_name(&self) -> &'static str {
        "qemu-fwcfg"
    }
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}
impl Migrate for FwCfg {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        let state = self.state.lock().unwrap();
        Box::new(migrate::FwCfgV1 {
            dma_addr: state.dma_addr(),
            selector: state.selector,
            offset: state.offset,
        })
    }
}

pub mod migrate {
    use serde::Serialize;

    #[derive(Serialize)]
    pub struct FwCfgV1 {
        pub dma_addr: u64,
        pub selector: u16,
        pub offset: u32,
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

    pub const FWCFG_FILENAME_LEN: usize = 56;
}
