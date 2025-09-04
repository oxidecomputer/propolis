// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{btree_map, BTreeMap};
use std::io::Write;
use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::accessors::MemAccessor;
use crate::common::*;
use crate::hw::qemu::ramfb::RamFb;
use crate::migrate::*;
use crate::pio::{PioBus, PioFn};
use crate::vmm::MemCtx;
use bits::*;

use thiserror::Error;
use zerocopy::IntoBytes;

const SIGNATURE_VALUE: &[u8; 4] = b"QEMU";

#[allow(unused)]
#[derive(Copy, Clone)]
#[repr(u16)]
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
    const fn name(&self) -> &'static str {
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

#[derive(Debug)]
pub enum Entry {
    FileDir,
    RamFb,
    Bytes(Vec<u8>),
}
impl Entry {
    pub fn fixed_u32(value: u32) -> Self {
        Self::Bytes(value.to_le_bytes().to_vec())
    }
}

#[derive(Debug, Error)]
pub enum InsertError {
    #[error("invalid selector")]
    InvalidSelector,

    #[error("selector {0} already exists")]
    SelectorExists(u16),

    #[error("name {0:?} already in use")]
    NameExists(String),

    #[error("no capacity")]
    NoCapacity,
}

struct Directory {
    entries: BTreeMap<u16, (Entry, String)>,
    names: BTreeMap<String, u16>,
    next_named: u16,
}
impl Directory {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            names: BTreeMap::new(),
            next_named: ITEMS_FILE_START,
        }
    }
    fn insert(
        &mut self,
        selector: u16,
        name: String,
        entry: Entry,
    ) -> Result<(), InsertError> {
        #[allow(clippy::map_entry)]
        if selector == ITEM_INVALID {
            Err(InsertError::InvalidSelector)
        } else if self.names.contains_key(&name) {
            Err(InsertError::NameExists(name))
        } else if self.entries.contains_key(&selector) {
            Err(InsertError::SelectorExists(selector))
        } else {
            self.names.insert(name.clone(), selector);
            self.entries.insert(selector, (entry, name));
            if selector == self.next_named {
                self.next_named += 1;
            }
            Ok(())
        }
    }
    fn insert_legacy(
        &mut self,
        id: LegacyId,
        entry: Entry,
    ) -> Result<(), InsertError> {
        let name = id.name().to_owned();
        self.insert(id as u16, name, entry)
    }
    fn remove(&mut self, selector: u16) -> Option<Entry> {
        let (entry, name) = self.entries.remove(&selector)?;
        let name_to_selector = self.names.remove(&name);
        assert_eq!(Some(selector), name_to_selector);
        if (ITEMS_FILE_START..ITEMS_FILE_END).contains(&selector) {
            self.next_named = u16::min(self.next_named, selector);
        }
        Some(entry)
    }
    /// Find the next available selector "slot" for a named entry
    fn next_named_selector(&self) -> Option<u16> {
        // Assume that consumers will be relatively well-behaved, and not be
        // adding/removing entries with reckless abandon.  This naive search can
        // be improved later if it becomes a problem.
        (self.next_named..ITEMS_FILE_END)
            .find(|selector| !self.entries.contains_key(&selector))
    }
    fn entry(&mut self, selector: u16) -> Option<&mut Entry> {
        self.entries.get_mut(&selector).map(|(ent, _name)| ent)
    }
    /// Look up (by `name`) the selector for an entry, if present
    fn named_selector(&mut self, name: &str) -> Option<u16> {
        self.names.get(name).copied()
    }
    fn entries(&self) -> Entries<'_> {
        Entries { iter: self.names.iter(), entries: &self.entries }
    }
    fn clear(&mut self) {
        self.entries.clear();
        self.names.clear();
        self.next_named = ITEMS_FILE_START;
    }
    /// Render the directory into the format expected by a guest reading it via
    /// the `fw_cfg` interface.
    fn render(&self) -> Vec<u8> {
        let rendered_size =
            size_of::<u32>() + self.entries.len() * size_of::<FwCfgFile>();
        let mut buf: Vec<u8> = Vec::with_capacity(rendered_size);
        buf.write_all(&(self.entries.len() as u32).to_be_bytes()).unwrap();

        for (selector, name, entry) in self.entries() {
            let size = match entry {
                Entry::FileDir => rendered_size,
                Entry::RamFb => RamFb::FWCFG_ENTRY_SIZE,
                Entry::Bytes(buf) => buf.len(),
            };
            let entry = FwCfgFile::new(size as u32, selector, name);
            buf.write_all(entry.as_bytes()).unwrap();
        }
        assert_eq!(buf.len(), rendered_size);
        buf
    }
}

#[derive(thiserror::Error, Debug)]
enum FwCfgErr {
    #[error("No entry selected")]
    NoneSelected,
    #[error("Entry is read-only")]
    ReadOnly,
    #[error("Bad DMA address")]
    BadAddr,
    #[error("DMA command not recognized")]
    UnrecognizedDmaCmd,
    #[error("Operation was not successful")]
    OpUnsuccessful,
}

struct Entries<'a> {
    iter: btree_map::Iter<'a, String, u16>,
    entries: &'a BTreeMap<u16, (Entry, String)>,
}
impl<'a> Iterator for Entries<'a> {
    type Item = (u16, &'a str, &'a Entry);

    fn next(&mut self) -> Option<Self::Item> {
        let (name, selector) = self.iter.next()?;
        let (entry, _name) = self.entries.get(selector).unwrap();
        debug_assert_eq!(name, _name);
        Some((*selector, name, entry))
    }
}

struct State {
    directory: Directory,

    selected: Option<SelectedEntry>,

    dma_addr_high: u32,
    dma_addr_low: u32,

    /// RamFB device associated with any [Entry::RamFb] type entry(s)
    ramfb: Option<Arc<RamFb>>,
}
impl State {
    fn dma_addr(&self) -> u64 {
        (u64::from(self.dma_addr_high) << 32) | u64::from(self.dma_addr_low)
    }
    fn reset(&mut self) {
        self.selected = None;
        self.dma_addr_high = 0;
        self.dma_addr_low = 0;
    }
}
struct SelectedEntry {
    selector: u16,
    offset: u32,
    cached_value: Option<Vec<u8>>,
}

pub struct FwCfg {
    state: Mutex<State>,
    acc_mem: MemAccessor,
}
impl FwCfg {
    /// Create a new `fw_cfg` device and populate it with the basic minimum
    /// entries ([LegacyId::Signature], [LegacyId::Id], [LegacyId::FileDir]).
    pub fn new() -> Arc<Self> {
        let mut directory = Directory::new();

        directory
            .insert_legacy(
                LegacyId::Signature,
                Entry::Bytes(SIGNATURE_VALUE.to_vec()),
            )
            .unwrap();
        directory
            .insert_legacy(
                LegacyId::Id,
                Entry::fixed_u32(FW_CFG_VER_BASE | FW_CFG_VER_DMA),
            )
            .unwrap();
        directory.insert_legacy(LegacyId::FileDir, Entry::FileDir).unwrap();

        Arc::new(Self {
            state: Mutex::new(State {
                directory,
                selected: None,

                dma_addr_high: 0,
                dma_addr_low: 0,

                ramfb: None,
            }),
            acc_mem: MemAccessor::new_orphan(),
        })
    }

    pub fn attach(self: &Arc<Self>, pio: &PioBus, acc_mem: &MemAccessor) {
        acc_mem.adopt(&self.acc_mem, Some("fw_cfg".to_string()));
        let ports = [
            (FW_CFG_IOP_SELECTOR, 1),
            (FW_CFG_IOP_DATA, 1),
            (FW_CFG_IOP_DMA_HI, 4),
            (FW_CFG_IOP_DMA_LO, 4),
        ];
        let this = self.clone();
        let piofn = Arc::new(move |port: u16, rwo: RWOp| match rwo {
            RWOp::Read(ro) => this.pio_read(port, ro),
            RWOp::Write(wo) => this.pio_write(port, wo),
        }) as Arc<PioFn>;
        for (port, len) in ports.iter() {
            pio.register(*port, *len, piofn.clone()).unwrap()
        }
    }

    /// Change the [RamFb] attachment for any [Entry::RamFb] entry(s)
    pub fn attach_ramfb(
        &self,
        ramfb: Option<Arc<RamFb>>,
    ) -> Option<Arc<RamFb>> {
        let mut state = self.state.lock().unwrap();
        std::mem::replace(&mut state.ramfb, ramfb)
    }

    /// Insert entry using [LegacyId] identifier (and its appropriately derived
    /// name)
    pub fn insert_legacy(
        &self,
        id: LegacyId,
        entry: Entry,
    ) -> Result<(), InsertError> {
        let mut state = self.state.lock().unwrap();
        state.directory.insert_legacy(id, entry)
    }
    /// Insert entry with specified `name`
    ///
    /// Note: Per the qemu docs for `fw_cfg`, the chosen `name` should be ASCII
    pub fn insert_named(
        &self,
        name: &str,
        entry: Entry,
    ) -> Result<u16, InsertError> {
        let mut state = self.state.lock().unwrap();
        let selector = state
            .directory
            .next_named_selector()
            .ok_or(InsertError::NoCapacity)?;
        state.directory.insert(selector, name.to_owned(), entry)?;
        Ok(selector)
    }

    pub fn remove(&self, selector: u16) -> Option<Entry> {
        let mut state = self.state.lock().unwrap();
        let entry = state.directory.remove(selector)?;
        Self::ensure_valid_selected(&mut state);
        Some(entry)
    }
    pub fn remove_named(&self, name: &str) -> Option<Entry> {
        let mut state = self.state.lock().unwrap();
        let selector = state.directory.named_selector(name)?;
        let entry = state
            .directory
            .remove(selector)
            .expect("entry is present for translated selector");
        Self::ensure_valid_selected(&mut state);
        Some(entry)
    }

    /// Ensure that the selected entry (if any) is still valid after a change to
    /// the directory, clearing the selection if there was trouble.
    ///
    /// This should not happen unless the VMM chooses to remove an item which
    /// the running guest had selected.  Doing so is rather unsporting.
    fn ensure_valid_selected(state: &mut MutexGuard<State>) {
        let selector = match state.selected.as_ref() {
            None => {
                return;
            }
            Some(s) => s.selector,
        };
        if state.directory.entry(selector).is_none() {
            state.selected = None;
        }
    }

    fn pio_read(&self, port: u16, ro: &mut ReadOp) {
        let mut state = self.state.lock().unwrap();
        match port {
            FW_CFG_IOP_SELECTOR => {
                if ro.len() == 2 {
                    let selector = state
                        .selected
                        .as_ref()
                        .map(|s| s.selector)
                        .unwrap_or(ITEM_INVALID);
                    ro.write_u16(selector);
                } else {
                    ro.fill(0);
                }
            }
            FW_CFG_IOP_DATA => {
                if ro.len() != 1 {
                    ro.fill(0);
                    return;
                }

                match self.read(&mut state, &mut ReadOp::new_child(0, ro, 0..1))
                {
                    Ok(1) => {}
                    Ok(_) | Err(_) => {
                        ro.write_u8(0);
                    }
                }
            }
            FW_CFG_IOP_DMA_HI => {
                if ro.len() == 4 {
                    ro.write_u32(state.dma_addr_high.to_be());
                } else {
                    ro.fill(0);
                }
            }
            FW_CFG_IOP_DMA_LO => {
                if ro.len() == 4 {
                    ro.write_u32(state.dma_addr_low.to_be());
                } else {
                    ro.fill(0);
                }
            }
            _ => {
                panic!("unexpected port {:x}", port);
            }
        }
    }

    fn pio_write(&self, port: u16, wo: &mut WriteOp) {
        let mut state = self.state.lock().unwrap();
        match port {
            FW_CFG_IOP_SELECTOR => {
                if wo.len() == 2 {
                    self.select(&mut state, wo.read_u16())
                }
            }
            FW_CFG_IOP_DATA => {
                // Writes through the legacy (non-DMA) interface are not
                // supported, and thus ignored.
            }
            FW_CFG_IOP_DMA_HI => {
                if wo.len() == 4 {
                    state.dma_addr_high = u32::from_be(wo.read_u32());
                }
            }
            FW_CFG_IOP_DMA_LO => {
                if wo.len() == 4 {
                    state.dma_addr_low = u32::from_be(wo.read_u32());
                    let _ = self.dma_initiate(&mut state);
                }
            }
            _ => {
                panic!("unexpected port {:x}", port);
            }
        }
    }

    fn select(&self, state: &mut MutexGuard<State>, selector: u16) {
        let _ = state.selected.take();
        if let Some(entry) = state.directory.entry(selector) {
            let value_buffer = match entry {
                // Cache the rendered file directory, if selected
                Entry::FileDir => Some(state.directory.render()),
                Entry::RamFb | Entry::Bytes(_) => None,
            };
            state.selected = Some(SelectedEntry {
                selector,
                offset: 0,
                cached_value: value_buffer,
            });
        }
    }

    fn read(
        &self,
        state: &mut MutexGuard<State>,
        ro: &mut ReadOp,
    ) -> Result<usize, FwCfgErr> {
        let state = state.deref_mut();

        // Reads to a non-existent entry result in no emitted bytes (and the
        // caller filling the remaining buffer with zeros).  This is in contrast
        // to attempted writes to missing entries resulting in a hard error.
        if state.selected.is_none() {
            return Ok(0);
        }
        let selected = state.selected.as_mut().unwrap();

        let entry = state
            .directory
            .entry(selected.selector)
            .expect("selected entry is present");
        // Encode the current offset into the ReadOp (as a child)
        let mut ro = ReadOp::new_child(selected.offset as usize, ro, ..);

        fn write_buf(buf: &[u8], ro: &mut ReadOp) -> usize {
            let off = ro.offset();
            if off >= buf.len() {
                0
            } else {
                let remain = &buf[off..];
                let copy_len = usize::min(remain.len(), ro.avail());

                ro.write_bytes(&remain[..copy_len]);
                copy_len
            }
        }

        let len = if let Some(buf) = selected.cached_value.as_ref() {
            write_buf(buf, &mut ro)
        } else {
            match entry {
                Entry::RamFb => {
                    if let Some(ramfb) = state.ramfb.as_ref() {
                        ramfb
                            .fwcfg_rw(RWOp::Read(&mut ro))
                            .map_err(|_| FwCfgErr::OpUnsuccessful)?;
                        ro.bytes_written()
                    } else {
                        0
                    }
                }
                Entry::Bytes(buf) => write_buf(&buf, &mut ro),
                Entry::FileDir => {
                    panic!("expected intact cached buffer for static entry");
                }
            }
        };
        selected.offset = selected
            .offset
            .checked_add(len as u32)
            .expect("offset does not overflow");
        Ok(len)
    }

    fn write(
        &self,
        state: &mut MutexGuard<State>,
        wo: &mut WriteOp,
    ) -> Result<usize, FwCfgErr> {
        let state = state.deref_mut();
        let selected = state.selected.as_mut().ok_or(FwCfgErr::NoneSelected)?;
        let entry = state
            .directory
            .entry(selected.selector)
            .expect("selected entry is present");
        // Encode the current offset into the WriteOp (as a child)
        let mut wo = WriteOp::new_child(selected.offset as usize, wo, ..);

        let len = match entry {
            Entry::FileDir | Entry::Bytes(_) => Err(FwCfgErr::ReadOnly),
            Entry::RamFb => {
                if let Some(ramfb) = state.ramfb.as_ref() {
                    ramfb
                        .fwcfg_rw(RWOp::Write(&mut wo))
                        .map_err(|_| FwCfgErr::OpUnsuccessful)?;
                    Ok(wo.bytes_read())
                } else {
                    Ok(0)
                }
            }
        }?;
        selected.offset = selected
            .offset
            .checked_add(len as u32)
            .expect("offset does not overflow");
        Ok(len)
    }

    fn dma_initiate(
        &self,
        state: &mut MutexGuard<State>,
    ) -> Result<(), FwCfgErr> {
        // initiating a DMA transfer clears the addr contents
        let addr = state.dma_addr();
        state.dma_addr_high = 0;
        state.dma_addr_low = 0;

        let mem_guard = self.acc_mem.access().expect("usable mem accessor");
        let mem = mem_guard.deref();
        let req: GuestData<FwCfgDmaAccess> =
            mem.read(GuestAddr(addr)).ok_or(FwCfgErr::BadAddr)?;

        fn dma_write_result(
            is_success: bool,
            req_addr: GuestAddr,
            mem: &MemCtx,
        ) {
            let result_val = match is_success {
                true => 0,
                false => u32::to_be(FwCfgDmaCtrl::ERROR.bits()),
            };
            mem.write(req_addr, &result_val);
        }

        // Do we recognize all of the control bits?
        //
        // The upper 16 bits are masked out, as they will contain the entry
        // selector when the SELECT function is specified.
        if FwCfgDmaCtrl::from_bits(req.ctrl.get() & 0xffff).is_none() {
            dma_write_result(false, GuestAddr(addr), mem);
            return Err(FwCfgErr::UnrecognizedDmaCmd);
        }

        let res = self.dma_operation(state, req, mem);

        dma_write_result(res.is_ok(), GuestAddr(addr), mem);
        Ok(())
    }

    fn dma_operation(
        &self,
        state: &mut MutexGuard<State>,
        req: GuestData<FwCfgDmaAccess>,
        mem: &MemCtx,
    ) -> Result<(), FwCfgErr> {
        let opts = FwCfgDmaCtrl::from_bits_truncate(req.ctrl.get());
        if opts.contains(FwCfgDmaCtrl::SELECT) {
            let selector = (req.ctrl.get() >> 16) as u16;
            self.select(state, selector);
        }

        // The expressed precedence of the available operations here is entirely
        // intentional.  Per the (paraphrased) fw_cfg documentation in qemu:
        //
        // - If the READ bit is set, a read operation will be performed
        // - If the WRITE bit is set (and not READ), a write operation will
        //   be performed
        // - If the SKIP bit is set (and neither READ nor WRITE), a skip
        //   operation will be performed
        if opts.contains(FwCfgDmaCtrl::READ) {
            let buf_len = req.len.get() as usize;
            let map = mem
                .writable_region(&GuestRegion(
                    GuestAddr(req.addr.get()),
                    buf_len,
                ))
                .ok_or(FwCfgErr::BadAddr)?;
            let mut ro = ReadOp::from_mapping(0, map);

            let nread = self.read(state, &mut ro)?;
            if nread < buf_len {
                // If the item being read did not cover the entire DMA region in
                // the request, zero out the rest
                assert!(ro.avail() > 0);
                ro.fill(0);
            }
        } else if opts.contains(FwCfgDmaCtrl::WRITE) {
            let buf_len = req.len.get() as usize;
            let map = mem
                .readable_region(&GuestRegion(
                    GuestAddr(req.addr.get()),
                    buf_len,
                ))
                .ok_or(FwCfgErr::BadAddr)?;
            let mut wo = WriteOp::from_mapping(0, map);
            self.write(state, &mut wo)?;
        } else if opts.contains(FwCfgDmaCtrl::SKIP) {
            if let Some(selected) = state.selected.as_mut() {
                selected.offset = selected.offset.saturating_add(req.len.get());
            }
        }

        Ok(())
    }
}

impl Lifecycle for FwCfg {
    fn type_name(&self) -> &'static str {
        "qemu-fwcfg"
    }
    fn migrate(&self) -> Migrator<'_> {
        Migrator::Single(self)
    }
    fn reset(&self) {
        self.state.lock().unwrap().reset();
    }
}
impl MigrateSingle for FwCfg {
    fn export(
        &self,
        _ctx: &MigrateCtx,
    ) -> Result<PayloadOutput, MigrateStateError> {
        let state = self.state.lock().unwrap();
        let selected =
            state.selected.as_ref().map(|sel| migrate::FwCfgSelectedV2 {
                selector: sel.selector,
                offset: sel.offset,
                cached_value: sel.cached_value.clone(),
            });
        let entries = state
            .directory
            .entries()
            .map(|(selector, name, entry)| migrate::FwCfgEntryV2 {
                selector,
                name: name.to_owned(),
                value: entry.into(),
            })
            .collect::<Vec<_>>();

        Ok(migrate::FwCfgV2 { dma_addr: state.dma_addr(), selected, entries }
            .into())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let mut data: migrate::FwCfgV2 = offer.parse()?;

        let mut state = self.state.lock().unwrap();
        state.dma_addr_low = data.dma_addr as u32;
        state.dma_addr_high = (data.dma_addr >> 32) as u32;
        state.selected = data.selected.take().map(|s| SelectedEntry {
            selector: s.selector,
            offset: s.offset,
            cached_value: s.cached_value,
        });

        state.directory.clear();
        for migrate::FwCfgEntryV2 { selector, name, value } in data.entries {
            state.directory.insert(selector, name, value.into()).map_err(
                |e| {
                    MigrateStateError::ImportFailed(format!(
                        "error importing fwcfg entry: {e:?}"
                    ))
                },
            )?;
        }
        Self::ensure_valid_selected(&mut state);
        Ok(())
    }
}

pub mod migrate {
    use crate::migrate::*;

    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct FwCfgV2 {
        pub dma_addr: u64,
        pub selected: Option<FwCfgSelectedV2>,
        pub entries: Vec<FwCfgEntryV2>,
    }
    #[derive(Deserialize, Serialize)]
    pub struct FwCfgSelectedV2 {
        pub selector: u16,
        pub offset: u32,
        pub cached_value: Option<Vec<u8>>,
    }
    #[derive(Deserialize, Serialize)]
    pub struct FwCfgEntryV2 {
        pub selector: u16,
        pub name: String,
        pub value: FwCfgEntryValueV2,
    }
    #[derive(Deserialize, Serialize, Clone)]
    pub enum FwCfgEntryValueV2 {
        FileDir,
        RamFb,
        Bytes(Vec<u8>),
    }
    impl From<&super::Entry> for FwCfgEntryValueV2 {
        fn from(value: &super::Entry) -> Self {
            match value {
                super::Entry::FileDir => Self::FileDir,
                super::Entry::RamFb => Self::RamFb,
                super::Entry::Bytes(buf) => Self::Bytes(buf.clone()),
            }
        }
    }
    impl From<FwCfgEntryValueV2> for super::Entry {
        fn from(value: FwCfgEntryValueV2) -> Self {
            match value {
                FwCfgEntryValueV2::FileDir => Self::FileDir,
                FwCfgEntryValueV2::RamFb => Self::RamFb,
                FwCfgEntryValueV2::Bytes(buf) => Self::Bytes(buf),
            }
        }
    }

    impl Schema<'_> for FwCfgV2 {
        fn id() -> SchemaId {
            ("qemu-fwcfg", 2)
        }
    }
}

mod bits {
    #![allow(unused)]

    use zerocopy::byteorder::big_endian::{
        U16 as BE16, U32 as BE32, U64 as BE64,
    };
    use zerocopy::{FromBytes, Immutable, IntoBytes};

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

    #[derive(IntoBytes, Immutable)]
    #[repr(C)]
    pub struct FwCfgFile {
        size: BE32,
        select: BE16,
        reserved: u16,
        name: [u8; FWCFG_FILENAME_LEN],
    }
    impl FwCfgFile {
        pub fn new(size: u32, select: u16, name: &str) -> Self {
            let name_len = name.len();
            assert!(name_len < FWCFG_FILENAME_LEN);

            let mut this = Self {
                size: BE32::new(size),
                select: BE16::new(select),
                reserved: 0,
                name: [0; FWCFG_FILENAME_LEN],
            };
            this.name[..name_len].copy_from_slice(name.as_bytes());
            this
        }
    }

    #[derive(IntoBytes, Default, Copy, Clone, Debug, FromBytes)]
    #[repr(C)]
    pub struct FwCfgDmaAccess {
        pub ctrl: BE32,
        pub len: BE32,
        pub addr: BE64,
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::accessors::MemAccessor;
    use crate::common::GuestAddr;
    use crate::vmm::Machine;

    use zerocopy::{FromBytes, Immutable, IntoBytes};

    fn pio_write<T: IntoBytes + Immutable>(dev: &FwCfg, port: u16, data: T) {
        let buf = data.as_bytes();
        let mut wo = WriteOp::from_buf(0, buf);
        dev.pio_write(port, &mut wo);
    }
    fn pio_read<T: IntoBytes + Immutable + FromBytes + Copy + Default>(
        dev: &FwCfg,
        port: u16,
    ) -> T {
        let mut val = T::default();
        let mut ro = ReadOp::from_buf(0, val.as_mut_bytes());
        dev.pio_read(port, &mut ro);
        drop(ro);
        val
    }
    fn pio_read_data<T: IntoBytes + FromBytes + Copy + Default>(
        dev: &FwCfg,
    ) -> T {
        let mut val = T::default();
        for c in val.as_mut_bytes().iter_mut() {
            *c = pio_read(dev, FW_CFG_IOP_DATA);
        }
        val
    }

    #[test]
    fn struct_sizing() {
        assert_eq!(std::mem::size_of::<FwCfgFile>(), 64);
        assert_eq!(std::mem::size_of::<FwCfgDmaAccess>(), 16);
    }

    #[test]
    fn pio_read_basic() {
        let dev = FwCfg::new();

        pio_write(&dev, FW_CFG_IOP_SELECTOR, LegacyId::Signature as u16);
        let rbuf = pio_read_data::<[u8; 4]>(&dev);

        assert_eq!(&rbuf, "QEMU".as_bytes());

        pio_write(&dev, FW_CFG_IOP_SELECTOR, LegacyId::Id as u16);
        let _rbuf = pio_read_data::<[u8; 4]>(&dev);
    }
    #[test]
    fn pio_read_missing() {
        let dev = FwCfg::new();

        pio_write(&dev, FW_CFG_IOP_SELECTOR, 0xfffe);
        let rbuf = pio_read_data::<[u8; 4]>(&dev);
        // missing entry should just be all zeroes
        assert_eq!(rbuf, [0u8; 4]);
    }

    #[test]
    fn read_version() {
        let dev = FwCfg::new();

        pio_write(&dev, FW_CFG_IOP_SELECTOR, LegacyId::Id as u16);

        let rbuf = pio_read_data::<[u8; 4]>(&dev);
        let version = u32::from_ne_bytes(rbuf);
        assert_eq!(version, FW_CFG_VER_BASE | FW_CFG_VER_DMA);
    }

    fn machine_setup() -> (Machine, Arc<FwCfg>, MemAccessor) {
        let machine = Machine::new_test().unwrap();

        let dev = FwCfg::new();
        dev.attach(&machine.bus_pio, &machine.acc_mem);

        let acc_mem = machine.acc_mem.child(None);

        (machine, dev, acc_mem)
    }

    struct DmaReq {
        ctrl: u32,
        len: u32,
        addr: u64,
    }
    fn write_dma_req(mem: &MemCtx, req_addr: u64, req: DmaReq) {
        mem.write(GuestAddr(req_addr), &u32::to_be(req.ctrl));
        mem.write(GuestAddr(req_addr + 4), &u32::to_be(req.len));
        mem.write(GuestAddr(req_addr + 8), &u64::to_be(req.addr));
    }
    fn submit_dma_req(dev: &FwCfg, req_addr: u64) {
        pio_write(dev, FW_CFG_IOP_DMA_HI, u32::to_be((req_addr >> 32) as u32));
        pio_write(dev, FW_CFG_IOP_DMA_LO, u32::to_be(req_addr as u32));
    }

    #[test]
    fn dma_read_basic() {
        let (_machine, dev, acc_mem) = machine_setup();
        let mem = acc_mem.access().unwrap();

        // Select signature entry and read 4 bytes
        let (req_addr, dma_addr) = (0x10_1000, 0x10_2000);
        write_dma_req(
            &mem,
            req_addr,
            DmaReq {
                ctrl: (u32::from(LegacyId::Signature as u16) << 16) | 0x000a,
                len: 4,
                addr: dma_addr,
            },
        );
        submit_dma_req(&dev, req_addr);

        // DMA should have successfully completed now
        assert_eq!(*mem.read::<u32>(GuestAddr(req_addr)).unwrap(), 0);
        let data = mem.read::<[u8; 4]>(GuestAddr(dma_addr)).unwrap();
        assert_eq!(&*data, "QEMU".as_bytes());
    }

    #[test]
    fn dma_read_missing() {
        let (_machine, dev, acc_mem) = machine_setup();
        let mem = acc_mem.access().unwrap();

        // Select missing entry and attempt to read 4 bytes
        let (req_addr, dma_addr) = (0x10_1000, 0x10_2000);
        write_dma_req(
            &mem,
            req_addr,
            DmaReq { ctrl: (0xfffe << 16) | 0x000a, len: 4, addr: dma_addr },
        );

        // Put garbage at dma destination to confirm it gets overwritten
        mem.write(GuestAddr(dma_addr), &[0xffu8; 4]);

        submit_dma_req(&dev, req_addr);

        // DMA should have successfully completed now
        assert_eq!(*mem.read::<u32>(GuestAddr(req_addr)).unwrap(), 0);
        let data = mem.read::<[u8; 4]>(GuestAddr(dma_addr)).unwrap();
        assert_eq!(*data, [0u8; 4]);
    }

    #[test]
    fn state_cleared_on_reset() {
        let (_machine, dev, _acc_mem) = machine_setup();

        // select an item
        pio_write(&dev, FW_CFG_IOP_SELECTOR, LegacyId::Id as u16);

        // ... and write the high DMA field
        // (Since the low field would initiate the op)
        let dma_val = 0x1234_5678;
        pio_write(&dev, FW_CFG_IOP_DMA_HI, dma_val);

        // Confirm those were set
        assert_eq!(
            LegacyId::Id as u16,
            pio_read::<u16>(&dev, FW_CFG_IOP_SELECTOR)
        );
        assert_eq!(dma_val, pio_read::<u32>(&dev, FW_CFG_IOP_DMA_HI));

        dev.reset();

        //... and are cleared after the reset
        assert_eq!(
            bits::ITEM_INVALID,
            pio_read::<u16>(&dev, FW_CFG_IOP_SELECTOR)
        );
        assert_eq!(0, pio_read::<u32>(&dev, FW_CFG_IOP_DMA_HI));
    }
}

pub mod formats {
    use super::Entry;
    use crate::hw::pci;
    use zerocopy::{Immutable, IntoBytes};

    /// A type for a range described in an E820 map entry.
    ///
    /// This is canonically defined as the ACPI "Address Range Types", though we
    /// only define the types we use, which are a subset of the types that EDK2
    /// is known to care about, which itself is a subset of types that ACPI and
    /// OEMs define or guest OSes may care about.
    #[derive(IntoBytes, Immutable)]
    #[repr(u32)]
    enum EfiAcpiMemoryType {
        Memory = 1,
        Reserved = 2,
        // For reference, though these types are unused.
        // Acpi = 3,
        // Nvs = 4,
    }

    /// One address/length/type entry in the E820 map.
    ///
    /// This is... almost defined by ACPI's "Address Range Descriptor Structure"
    /// table, under "INT 15H, E820". Critically, ACPI defines this structure
    /// with an additional "Extended Attributes" field which EDK2 does not know
    /// about and so we do not provide. Consequently the size of this struct is
    /// 20 bytes as defined in `OvmfPkg/Include/IndustryStandard/E820.h` rather
    /// than the ACPI definition's 24 bytes.
    #[derive(IntoBytes, Immutable)]
    #[repr(C, packed)]
    struct E820Entry64 {
        base_addr: u64,
        length: u64,
        ty: EfiAcpiMemoryType,
    }

    /// A list of E820 memory map entries.
    ///
    /// This is not defined by ACPI, but is an EDK2 implementation of a QMEU
    /// construct to communicate an E820 map to the firmware. It is parsed by
    /// EDK2 and added to its EFI memory map; it is not, itself, the memory map
    /// that OVMF presents via UEFI services. It is not required to be sorted,
    /// and EDK2 ignores entries starting below 4 GiB. Adding additional
    /// low-memory entries is not harmful, but not valuable to EDK2 either.
    pub struct E820Table(Vec<E820Entry64>);
    impl E820Table {
        pub fn new() -> Self {
            Self(Vec::new())
        }

        /// Add an address range corresponding to usable memory.
        pub fn add_mem(&mut self, base_addr: u64, length: u64) {
            self.0.push(E820Entry64 {
                base_addr,
                length,
                ty: EfiAcpiMemoryType::Memory,
            });
        }

        /// Add a reserved address, not to be used by the guest OS.
        pub fn add_reserved(&mut self, base_addr: u64, length: u64) {
            self.0.push(E820Entry64 {
                base_addr,
                length,
                ty: EfiAcpiMemoryType::Reserved,
            });
        }

        pub fn finish(self) -> Entry {
            Entry::Bytes(self.0.as_bytes().to_vec())
        }
    }

    #[cfg(test)]
    mod test_e820 {
        use super::{E820Entry64, E820Table};
        use crate::hw::qemu::fwcfg::Entry;

        #[test]
        fn entry_size_is_correct() {
            // Compare the size of our definition of an E820 to EDK2's
            // definition. EDK2 interprets our provided bytes by its definition,
            // so they must match.
            assert_eq!(std::mem::size_of::<E820Entry64>(), 20);
        }

        #[test]
        fn basic() {
            let mut e820_table = E820Table::new();

            // Arbitrary bit patterns here, just to make eyeballing the layout
            // more straightforward.
            //
            // Also note the E820 table itself does not check if ranges overlap.
            // In practice it is directly constructed from an ASpace, which does
            // perform those checks.
            e820_table.add_mem(0x0102_0304_0506_0010, 0x1122_3344_5566_7788);
            e820_table
                .add_reserved(0x0102_0304_0506_fff0, 0xffee_ddcc_bbaa_9988);

            // We also don't require the E820 map to be ordered. ACPI does not
            // imply that it should be, nor do EDK2 or guest OSes, even though
            // entries are often enumerated in address order.
            e820_table.add_mem(0x0102_0304_0506_0000, 0x1122_3344_5566_7799);

            // rustfmt::skip here and below because eight bytes per line helps
            // eyeball with the entries as written above. rustfmt would try to
            // fit ten bytes per row to pack the 80-column width and that's just
            // annoying here.
            #[rustfmt::skip]
            const FIRST_ENTRY: [u8; 20] = [
                0x10, 0x00, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
                0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
                0x01, 0x00, 0x00, 0x00,
            ];

            #[rustfmt::skip]
            const SECOND_ENTRY: [u8; 20] = [
                0xf0, 0xff, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
                0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
                0x02, 0x00, 0x00, 0x00,
            ];

            #[rustfmt::skip]
            const THIRD_ENTRY: [u8; 20] = [
                0x00, 0x00, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
                0x99, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
                0x01, 0x00, 0x00, 0x00,
            ];

            let entry = e820_table.finish();
            let Entry::Bytes(bytes) = entry else {
                panic!("entry did not produce bytes, but instead {:?}", entry);
            };

            let expected_size =
                FIRST_ENTRY.len() + SECOND_ENTRY.len() + THIRD_ENTRY.len();
            assert_eq!(bytes.len(), expected_size);

            let tests = [
                (&bytes[0..20], &FIRST_ENTRY, "First E820 entry"),
                (&bytes[20..40], &SECOND_ENTRY, "Second E820 entry"),
                (&bytes[40..60], &THIRD_ENTRY, "Third E820 entry"),
            ];

            for (actual, expected, entry_name) in tests.iter() {
                assert_eq!(
                    actual, expected,
                    "{} contents are incorrect",
                    entry_name
                );
            }
        }
    }

    /// Collect one or more device elections for use in generating a boot order
    /// `fw_cfg` entry, suitable for consumption by OVMF bootrom.
    pub struct BootOrder(Vec<String>);
    impl BootOrder {
        pub fn new() -> Self {
            Self(Vec::new())
        }

        /// Add a generic disk
        pub fn add_disk(&mut self, loc: pci::BusLocation) {
            // The OVMF logic is looking for "scsi"
            let pci_path = Self::format_pci(loc, "scsi");
            self.0.push(format!("{pci_path}/disk@0,0"));
        }

        /// Add generic PCI device
        ///
        /// For example, one might add an entry for an ethernet NIC as such:
        /// ```
        /// # use propolis::hw::qemu::fwcfg::formats::BootOrder;
        /// # use propolis::hw::pci::BusLocation;
        /// # let mut bootorder = BootOrder::new();
        /// # let bus_loc = BusLocation::new(1, 0).unwrap();
        /// bootorder.add_pci(bus_loc, "ethernet");
        /// ```
        pub fn add_pci(&mut self, loc: pci::BusLocation, kind: &str) {
            self.0.push(Self::format_pci(loc, kind));
        }

        /// Add an NVMe disk.  This assumes namespace 1, as our NVMe emulation
        /// does not currently support multiple namespaces in a device.
        pub fn add_nvme(&mut self, loc: pci::BusLocation, eui: u64) {
            // The decoding in OVMF demands that the bootorder entry identify
            // nvme devices as vendor=0x8086 and device=5845.  While that
            // hardcoded logic exists, we must encode our entry to match, even
            // when the device in question may bear different identity info.
            //
            // For more details, see the TranslatePciOfwNodes() function in
            // OvmfPkg/Library/QemuBootOrderLib/QemuBootOrderLib.c.
            let pci_path = Self::format_pci(loc, "pci8086,5845");
            let ns = 1;
            self.0.push(format!("{pci_path}/namespace@{ns:x},{eui:x}"));
        }

        /// Render the contained boot order selections into a `fw_cfg` [Entry]
        pub fn finish(self) -> Entry {
            let Self(mut entries) = self;
            entries.push("HALT\0".to_owned());

            Entry::Bytes(entries.join("\n").to_string().into())
        }

        fn format_pci(loc: pci::BusLocation, name: &str) -> String {
            let (slot, func): (u8, u8) = (loc.dev.into(), loc.func.into());
            format!("/pci@i0cf8/{name}@{slot:x},{func:x}")
        }
    }

    #[cfg(test)]
    mod test_bootorder {
        use super::BootOrder;
        use crate::hw::pci::BusLocation;
        use crate::hw::qemu::fwcfg;

        #[test]
        fn basic() {
            let mut bo = BootOrder::new();

            bo.add_disk(BusLocation::new_unchecked(1, 2));
            bo.add_pci(BusLocation::new_unchecked(10, 3), "ethernet");
            bo.add_nvme(BusLocation::new_unchecked(31, 4), 0x123456789abcd);

            let raw = match bo.finish() {
                fwcfg::Entry::Bytes(v) => v,
                other => {
                    panic!("Unexpected entry type: {other:?}");
                }
            };
            let expected = [
                "/pci@i0cf8/scsi@1,2/disk@0,0",
                "/pci@i0cf8/ethernet@a,3",
                "/pci@i0cf8/pci8086,5845@1f,4/namespace@1,123456789abcd",
                // Trailing NUL is load-bearing
                "HALT\0",
            ];
            let entries = std::str::from_utf8(&raw)
                .expect("bootorder is valid utf8")
                .split('\n')
                .collect::<Vec<_>>();
            assert_eq!(&expected[..], &entries[..]);
        }
    }
}
