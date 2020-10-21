use std::mem;
use std::num::Wrapping;
use std::sync::atomic::{fence, Ordering};
use std::sync::Mutex;

use super::VirtioIntr;
use crate::common::*;
use crate::vmm::MemCtx;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const VIRTQ_DESC_F_INDIRECT: u16 = 4;

const VRING_AVAIL_F_NO_INTERRUPT: u16 = 1;
const VRING_USED_F_NO_NOTIFY: u16 = 1;

#[repr(C)]
#[derive(Copy, Clone)]
struct VqdDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct VqdUsed {
    id: u32,
    len: u32,
}

enum VqStatus {
    Init,
    Mapped,
    Error,
}

pub struct VqControl {
    status: VqStatus,
    pub(super) gpa_desc: GuestAddr,
}

struct VqAvail {
    valid: bool,
    gpa_flags: GuestAddr,
    gpa_idx: GuestAddr,
    gpa_ring: GuestAddr,
    cur_avail_idx: Wrapping<u16>,

    gpa_desc: GuestAddr,
}
impl VqAvail {
    fn read_next_avail(&mut self, rsize: u16, mem: &MemCtx) -> Option<u16> {
        if !self.valid {
            return None;
        }
        if let Some(idx) = mem.read::<u16>(self.gpa_idx) {
            let ndesc = Wrapping(idx) - self.cur_avail_idx;
            if ndesc.0 != 0 && ndesc.0 < rsize {
                let read_idx = self.cur_avail_idx.0 & (rsize - 1);
                self.cur_avail_idx += Wrapping(1);

                fence(Ordering::Acquire);
                let addr =
                    self.gpa_ring + (read_idx as usize * mem::size_of::<u16>());
                return mem.read(addr);
            }
        }
        None
    }
    fn read_ring_descr(
        &self,
        id: u16,
        rsize: u16,
        mem: &MemCtx,
    ) -> Option<VqdDesc> {
        assert!(id < rsize);
        let addr = self.gpa_desc + (id as usize * mem::size_of::<VqdDesc>());
        mem.read::<VqdDesc>(addr)
    }
}

struct VqUsed {
    valid: bool,
    gpa_flags: GuestAddr,
    gpa_idx: GuestAddr,
    gpa_ring: GuestAddr,
    used_idx: Wrapping<u16>,
    interrupt: Option<Box<dyn VirtioIntr>>,
}
impl VqUsed {
    fn write_used(&mut self, id: u16, len: u32, rsize: u16, mem: &MemCtx) {
        let idx = self.used_idx.0 & (rsize - 1);
        self.used_idx += Wrapping(1);
        let desc_addr =
            self.gpa_ring + (idx as usize * mem::size_of::<VqdUsed>());

        let used = VqdUsed { id: id as u32, len };
        mem.write(desc_addr, &used);

        fence(Ordering::Release);
        mem.write(self.gpa_idx, &self.used_idx.0);
    }
    fn suppress_intr(&self, mem: &MemCtx) -> bool {
        let flags: u16 = mem.read(self.gpa_flags).unwrap();
        flags & VRING_AVAIL_F_NO_INTERRUPT != 0
    }
}

pub struct VirtQueue {
    size: u16,
    pub(super) ctrl: Mutex<VqControl>,
    avail: Mutex<VqAvail>,
    used: Mutex<VqUsed>,
}
const LEGACY_QALIGN: u64 = PAGE_SIZE as u64;
fn qalign(addr: u64, align: u64) -> u64 {
    let mask = align - 1;
    (addr + mask) & !mask
}
impl VirtQueue {
    pub fn new(size: u16) -> Self {
        assert!(size.is_power_of_two());
        Self {
            size,
            ctrl: Mutex::new(VqControl {
                status: VqStatus::Init,
                gpa_desc: GuestAddr(0),
            }),
            avail: Mutex::new(VqAvail {
                valid: false,
                gpa_flags: GuestAddr(0),
                gpa_idx: GuestAddr(0),
                gpa_ring: GuestAddr(0),
                cur_avail_idx: Wrapping(0),
                gpa_desc: GuestAddr(0),
            }),
            used: Mutex::new(VqUsed {
                valid: false,
                gpa_flags: GuestAddr(0),
                gpa_idx: GuestAddr(0),
                gpa_ring: GuestAddr(0),
                used_idx: Wrapping(0),
                interrupt: None,
            }),
        }
    }
    pub(super) fn reset(&self) {
        let mut state = self.ctrl.lock().unwrap();
        let mut avail = self.avail.lock().unwrap();
        let mut used = self.used.lock().unwrap();

        // XXX verify no outstanding chains
        state.status = VqStatus::Init;
        state.gpa_desc = GuestAddr(0);
        avail.valid = false;
        used.valid = false;
        avail.cur_avail_idx = Wrapping(0);
        used.used_idx = Wrapping(0);
    }
    pub fn map_legacy(&self, addr: u64) -> bool {
        assert_eq!(addr & (LEGACY_QALIGN - 1), 0);
        let mut state = self.ctrl.lock().unwrap();
        let mut avail = self.avail.lock().unwrap();
        let mut used = self.used.lock().unwrap();

        // even if the map is unsuccessful, track the address provided
        state.gpa_desc = GuestAddr(addr);

        let size = self.size as usize;

        let desc_addr = addr;
        let desc_len = mem::size_of::<VqdDesc>() * size;
        let avail_addr = desc_addr + desc_len as u64;
        let avail_len = 2 * (size + 3);
        let used_addr = qalign(avail_addr + avail_len as u64, LEGACY_QALIGN);
        let _used_len = mem::size_of::<VqUsed>() * size + 2 * 3;

        avail.gpa_flags = GuestAddr(avail_addr);
        avail.gpa_idx = GuestAddr(avail_addr + 2);
        avail.gpa_ring = GuestAddr(avail_addr + 4);
        // The descriptor ring address is duplicated into the avail structure
        // so it can be accessed with only the one lock.
        avail.gpa_desc = GuestAddr(desc_addr);

        used.gpa_flags = GuestAddr(used_addr);
        used.gpa_idx = GuestAddr(used_addr + 2);
        used.gpa_ring = GuestAddr(used_addr + 4);

        state.status = VqStatus::Mapped;
        avail.valid = true;
        used.valid = true;

        true
    }
    pub fn avail_count(&self, mem: &MemCtx) -> u16 {
        let avail = self.avail.lock().unwrap();
        if !avail.valid {
            return 0;
        }
        if let Some(idx) = mem.read::<u16>(avail.gpa_idx) {
            let ndesc = Wrapping(idx) - avail.cur_avail_idx;
            if ndesc.0 != 0 && ndesc.0 < self.size {
                return ndesc.0;
            }
        }
        0
    }
    pub fn pop_avail(&self, chain: &mut Chain, mem: &MemCtx) -> Option<u32> {
        assert!(chain.idx.is_none());
        let mut avail = self.avail.lock().unwrap();
        let id = avail.read_next_avail(self.size, mem)?;

        let mut desc = avail.read_ring_descr(id, self.size, mem)?;
        let mut flags = DescFlag::from_bits_truncate(desc.flags);
        let mut count = 0;
        let mut len = 0;
        chain.idx = Some(id);

        // non-indirect descriptor(s)
        while !flags.contains(DescFlag::INDIRECT) {
            let buf = match flags.contains(DescFlag::WRITE) {
                true => ChainBuf::Writable(GuestAddr(desc.addr), desc.len),
                false => ChainBuf::Readable(GuestAddr(desc.addr), desc.len),
            };
            count += 1;
            len += desc.len;
            chain.push_buf(buf);

            if flags.intersects(DescFlag::NEXT | DescFlag::INDIRECT) {
                if count == self.size {
                    // XXX: signal error condition?
                    chain.idx = None;
                    return None;
                }
                if let Some(next) =
                    avail.read_ring_descr(desc.next, self.size, mem)
                {
                    desc = next;
                    flags = DescFlag::from_bits_truncate(desc.flags);
                } else {
                    return Some(len);
                }
            } else {
                return Some(len);
            }
        }
        // XXX: skip indirect if not negotiated
        if flags.contains(DescFlag::INDIRECT) {
            if (desc.len as usize) < mem::size_of::<VqdDesc>()
                || desc.len as usize & (mem::size_of::<VqdDesc>() - 1) != 0
            {
                // XXX: signal error condition?
                chain.idx = None;
                return None;
            }
            let indirect_count = desc.len as usize / mem::size_of::<VqdDesc>();
            let idescs = mem
                .read_many::<VqdDesc>(GuestAddr(desc.addr), indirect_count)
                .unwrap();
            desc = idescs.get(0).unwrap();
            flags = DescFlag::from_bits_truncate(desc.flags);
            loop {
                let buf = match flags.contains(DescFlag::WRITE) {
                    true => ChainBuf::Writable(GuestAddr(desc.addr), desc.len),
                    false => ChainBuf::Readable(GuestAddr(desc.addr), desc.len),
                };

                count += 1;
                len += desc.len;
                chain.push_buf(buf);

                if flags.contains(DescFlag::NEXT) {
                    // XXX: better error handling
                    desc = idescs.get(desc.next as usize).unwrap();
                    flags = DescFlag::from_bits_truncate(desc.flags);
                } else {
                    break;
                }
            }
        }
        Some(len)
    }
    pub fn push_used(&self, chain: &mut Chain, mem: &MemCtx) {
        assert!(chain.idx.is_some());
        let mut used = self.used.lock().unwrap();
        let id = mem::replace(&mut chain.idx, None).unwrap();
        // XXX: for now, just go off of the write stats
        let len = chain.write_stat.bytes - chain.write_stat.bytes_remain;
        used.write_used(id, len, self.size, mem);
        if !used.suppress_intr(mem) {
            used.interrupt.as_ref().map(|i| i.notify());
        }
        chain.reset();
    }

    pub(super) fn set_interrupt(&self, intr: Box<dyn VirtioIntr>) {
        let mut used = self.used.lock().unwrap();
        used.interrupt = Some(intr)
    }
}

bitflags! {
    #[derive(Default)]
    pub struct DescFlag: u16 {
        const NEXT = VIRTQ_DESC_F_NEXT;
        const WRITE = VIRTQ_DESC_F_WRITE;
        const INDIRECT = VIRTQ_DESC_F_INDIRECT;
    }
}

#[derive(Copy, Clone, Debug)]
pub enum ChainBuf {
    Readable(GuestAddr, u32),
    Writable(GuestAddr, u32),
}
impl ChainBuf {
    pub fn is_readable(&self) -> bool {
        match self {
            ChainBuf::Readable(_, _) => true,
            ChainBuf::Writable(_, _) => false,
        }
    }
    pub fn is_writable(&self) -> bool {
        !self.is_readable()
    }
}

#[derive(Default, Debug)]
struct ChainStat {
    count: u32,
    bytes: u32,
    bytes_remain: u32,
    pos_idx: u32,
    pos_off: u32,
}

#[derive(Debug)]
pub struct Chain {
    idx: Option<u16>,
    read_stat: ChainStat,
    write_stat: ChainStat,
    bufs: Vec<ChainBuf>,
}
impl Chain {
    pub fn with_capacity(size: usize) -> Self {
        assert!(size <= u16::MAX as usize);
        Self {
            idx: None,
            read_stat: Default::default(),
            write_stat: Default::default(),
            bufs: Vec::with_capacity(size),
        }
    }
    fn push_buf(&mut self, buf: ChainBuf) {
        let (mut stat, len) = match buf {
            ChainBuf::Readable(_, len) => (&mut self.read_stat, len),
            ChainBuf::Writable(_, len) => (&mut self.write_stat, len),
        };
        stat.count += 1;
        stat.bytes += len;
        stat.bytes_remain += len;
        self.bufs.push(buf);
    }
    fn reset(&mut self) {
        self.idx = None;
        self.read_stat = Default::default();
        self.write_stat = Default::default();
        self.bufs.clear();
    }

    pub fn read<T: Copy>(&mut self, item: &mut T, mem: &MemCtx) -> bool {
        let item_sz = mem::size_of::<T>();
        if (self.read_stat.bytes_remain as usize) < item_sz {
            return false;
        }
        // SAFETY: We assume the mutable item reference we have received is
        // valid (aligned, etc) to begin with.  It is cast into a u8 slice to
        // handle cases where it cannot be filled by a single buffer copy.
        let raw = unsafe {
            std::slice::from_raw_parts_mut(item as *mut T as *mut u8, item_sz)
        };
        let mut done = 0;
        let total = self.for_remaining_type(true, |addr, len| {
            let remain = &mut raw[done..];
            if let Some(copied) = mem.read_into(addr, remain, len) {
                let need_more = copied != remain.len();

                done += copied;
                (copied, need_more)
            } else {
                // Copy failed, so do not attempt anything else
                (0, false)
            }
        });
        total == item_sz
    }
    pub fn readable_buf(&mut self, limit: usize) -> Option<GuestRegion> {
        if limit == 0 || self.read_stat.bytes_remain == 0 {
            return None;
        }

        let mut res: Option<GuestRegion> = None;
        self.for_remaining_type(true, |addr, blen| {
            let to_consume = usize::min(blen, limit);

            res = Some(GuestRegion(addr, to_consume));
            (to_consume, false)
        });
        res
    }
    pub fn write<T: Copy>(&mut self, item: &T, mem: &MemCtx) -> bool {
        let item_sz = mem::size_of::<T>();
        if (self.write_stat.bytes_remain as usize) < item_sz {
            return false;
        }
        // SAFETY: We assume the item reference we have received is valid
        // (aligned, etc) to begin with.  It is cast into a u8 slice to handle
        // cases where it cannot be filled by a single buffer copy.
        let raw = unsafe {
            std::slice::from_raw_parts(item as *const T as *const u8, item_sz)
        };
        let mut done = 0;
        let total = self.for_remaining_type(false, |addr, len| {
            let remain = &raw[done..];
            if let Some(copied) = mem.write_from(addr, remain, len) {
                let need_more = copied != remain.len();

                done += copied;
                (copied, need_more)
            } else {
                // Copy failed, so do not attempt anything else
                (0, false)
            }
        });
        total == item_sz
    }

    pub fn write_skip(&mut self, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        if (self.write_stat.bytes_remain as usize) < len {
            return false;
        }
        let mut remain = len;
        self.for_remaining_type(false, |_addr, blen| {
            if blen < remain {
                // consume (skip) whole buffer length and continue
                (blen, true)
            } else {
                // consume only what is needed
                (remain, false)
            }
        });
        true
    }
    pub fn writable_buf(&mut self, limit: usize) -> Option<GuestRegion> {
        if limit == 0 || self.write_stat.bytes_remain == 0 {
            return None;
        }

        let mut res: Option<GuestRegion> = None;
        self.for_remaining_type(false, |addr, blen| {
            let to_consume = usize::min(blen, limit);

            res = Some(GuestRegion(addr, to_consume));
            (to_consume, false)
        });
        res
    }

    pub fn remain_write_bytes(&self) -> usize {
        self.write_stat.bytes_remain as usize
    }
    pub fn remain_read_bytes(&self) -> usize {
        self.read_stat.bytes_remain as usize
    }

    fn for_remaining_type<F>(&mut self, is_read: bool, mut f: F) -> usize
    where
        F: FnMut(GuestAddr, usize) -> (usize, bool),
    {
        let stat = match is_read {
            true => &mut self.read_stat,
            false => &mut self.write_stat,
        };
        let iter = self
            .bufs
            .iter()
            .enumerate()
            .skip(stat.pos_idx as usize)
            .skip_while(|(_i, buf)| {
                if is_read {
                    buf.is_writable()
                } else {
                    buf.is_readable()
                }
            });
        let mut consumed_total = 0;
        for (idx, buf) in iter {
            let (addr, len) = match buf {
                ChainBuf::Readable(a, l) => {
                    if !is_read {
                        continue;
                    }
                    (*a, *l)
                }
                ChainBuf::Writable(a, l) => {
                    if is_read {
                        continue;
                    }
                    (*a, *l)
                }
            };
            if len == 0 {
                // skip 0-len buffers, even though they should not exist
                continue;
            }
            assert!(stat.pos_off < len);
            let off_addr = GuestAddr(addr.0 + stat.pos_off as u64);
            let off_len = (len - stat.pos_off) as usize;
            let (consumed, do_more) = f(off_addr, off_len);
            assert!(consumed <= off_len);
            if consumed != 0 {
                consumed_total += consumed;
                if consumed == off_len {
                    stat.pos_idx = idx as u32 + 1;
                    stat.pos_off = 0;
                } else {
                    stat.pos_off += consumed as u32;
                }
            }
            if !do_more {
                break;
            }
        }
        assert!(consumed_total as u32 <= stat.bytes_remain);
        stat.bytes_remain -= consumed_total as u32;
        consumed_total
    }
}
