// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::mem;
use std::num::{NonZeroU16, Wrapping};
use std::ops::Index;
use std::slice::SliceIndex;
use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::bits::*;
use super::probes;
use super::{VirtioIntr, VqIntr};
use crate::accessors::MemAccessor;
use crate::common::*;
use crate::migrate::MigrateStateError;
use crate::vmm::MemCtx;

use zerocopy::FromBytes;

#[repr(C)]
#[derive(Copy, Clone, FromBytes)]
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

#[derive(Copy, Clone, Debug)]
pub struct VqReq {
    desc_idx: u16,
    avail_idx: u16,
}

pub struct VqAvail {
    /// Is populated with a valid physical address(es) for its contents
    valid: bool,

    gpa_flags: GuestAddr,
    gpa_idx: GuestAddr,
    gpa_ring: GuestAddr,
    cur_avail_idx: Wrapping<u16>,

    gpa_desc: GuestAddr,
}
impl VqAvail {
    /// If there's a request ready, pop it off the queue and return the
    /// corresponding descriptor and available ring indicies.
    fn read_next_avail(&mut self, rsize: u16, mem: &MemCtx) -> Option<VqReq> {
        if !self.valid {
            return None;
        }
        if let Some(idx) = mem.read::<u16>(self.gpa_idx) {
            let ndesc = Wrapping(*idx) - self.cur_avail_idx;
            if ndesc.0 != 0 && ndesc.0 < rsize {
                let avail_idx = self.cur_avail_idx.0 & (rsize - 1);
                self.cur_avail_idx += Wrapping(1);

                fence(Ordering::Acquire);
                let addr = self.gpa_ring.offset::<u16>(avail_idx as usize);
                return mem
                    .read(addr)
                    .map(|desc_idx| VqReq { desc_idx: *desc_idx, avail_idx });
            }
        }
        None
    }
    fn read_ring_descr(
        &self,
        id: u16,
        rsize: u16,
        mem: &MemCtx,
    ) -> Option<GuestData<VqdDesc>> {
        assert!(id < rsize);
        let addr = self.gpa_desc.offset::<VqdDesc>(id as usize);
        mem.read::<VqdDesc>(addr)
    }
    fn reset(&mut self) {
        self.valid = false;
        self.gpa_flags = GuestAddr(0);
        self.gpa_idx = GuestAddr(0);
        self.gpa_ring = GuestAddr(0);
        self.gpa_desc = GuestAddr(0);
        self.cur_avail_idx = Wrapping(0);
    }
    fn map_split(&mut self, desc_addr: u64, avail_addr: u64) {
        self.gpa_desc = GuestAddr(desc_addr);
        // 16-bit flags, followed by 16-bit idx, followed by avail desc ring
        self.gpa_flags = GuestAddr(avail_addr);
        self.gpa_idx = GuestAddr(avail_addr + 2);
        self.gpa_ring = GuestAddr(avail_addr + 4);
    }
}

pub struct VqUsed {
    /// Is populated with a valid physical address(es) for its contents
    valid: bool,

    gpa_flags: GuestAddr,
    gpa_idx: GuestAddr,
    gpa_ring: GuestAddr,
    used_idx: Wrapping<u16>,
    interrupt: Option<Box<dyn VirtioIntr>>,
}
impl VqUsed {
    fn write_used(&mut self, id: u16, len: u32, rsize: u16, mem: &MemCtx) {
        // We do not expect used entries to be pushed into a virtqueue which has
        // not been configured atop physical addresses yet.
        assert!(self.valid);

        let idx = self.used_idx.0 & (rsize - 1);
        self.used_idx += Wrapping(1);
        let desc_addr = self.gpa_ring.offset::<VqdUsed>(idx as usize);

        let used = VqdUsed { id: u32::from(id), len };
        mem.write(desc_addr, &used);

        fence(Ordering::Release);
        mem.write(self.gpa_idx, &self.used_idx.0);
    }
    fn intr_supressed(&self, mem: &MemCtx) -> bool {
        let flags: u16 = *mem.read(self.gpa_flags).unwrap();
        flags & VRING_AVAIL_F_NO_INTERRUPT != 0
    }
    fn reset(&mut self) {
        self.valid = false;
        self.gpa_flags = GuestAddr(0);
        self.gpa_idx = GuestAddr(0);
        self.gpa_ring = GuestAddr(0);
        self.used_idx = Wrapping(0);
    }
    fn map_split(&mut self, gpa: u64) {
        // 16-bit flags, followed by 16-bit idx, followed by used desc ring
        self.gpa_flags = GuestAddr(gpa);
        self.gpa_idx = GuestAddr(gpa + 2);
        self.gpa_ring = GuestAddr(gpa + 4);
    }
}

pub struct VirtQueue {
    pub id: u16,
    pub size: u16,
    pub live: AtomicBool,
    avail: Mutex<VqAvail>,
    used: Mutex<VqUsed>,
    pub acc_mem: MemAccessor,
}
const LEGACY_QALIGN: u64 = PAGE_SIZE as u64;
const fn qalign(addr: u64, align: u64) -> u64 {
    assert!(align.is_power_of_two());

    let mask = align - 1;
    (addr + mask) & !mask
}
impl VirtQueue {
    pub fn new(id: u16, size: u16) -> Self {
        assert!(size.is_power_of_two());
        Self {
            id,
            size,
            live: AtomicBool::new(false),
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
            acc_mem: MemAccessor::new_orphan(),
        }
    }
    pub(super) fn reset(&self) {
        let mut avail = self.avail.lock().unwrap();
        let mut used = self.used.lock().unwrap();

        // XXX verify no outstanding chains
        avail.reset();
        used.reset();
        self.live.store(false, Ordering::Release);
    }

    /// Attempt to establish ring mappings at a specified physical address,
    /// using legacy-style split virtqueue layout.
    ///
    /// `addr` must be aligned to 4k per the legacy requirements
    pub fn map_legacy(&self, addr: u64) {
        assert_eq!(addr & (LEGACY_QALIGN - 1), 0);

        let size = self.size as usize;

        let desc_addr = addr;
        let desc_len = mem::size_of::<VqdDesc>() * size;

        let avail_addr = desc_addr + desc_len as u64;
        let avail_len = 2 * (size + 3);

        let used_addr = qalign(avail_addr + avail_len as u64, LEGACY_QALIGN);
        let _used_len = mem::size_of::<VqUsed>() * size + 2 * 3;

        let mut avail = self.avail.lock().unwrap();
        let mut used = self.used.lock().unwrap();
        avail.map_split(desc_addr, avail_addr);
        used.map_split(used_addr);
        avail.valid = true;
        used.valid = true;
    }
    pub fn get_state(&self) -> Info {
        let avail = self.avail.lock().unwrap();
        let used = self.used.lock().unwrap();

        Info {
            mapping: MapInfo {
                desc_addr: avail.gpa_desc.0,
                avail_addr: avail.gpa_flags.0,
                used_addr: used.gpa_flags.0,
                valid: avail.valid,
            },
            avail_idx: avail.cur_avail_idx.0,
            used_idx: used.used_idx.0,
        }
    }
    pub fn set_state(&self, info: &Info) {
        let mut avail = self.avail.lock().unwrap();
        let mut used = self.used.lock().unwrap();

        avail.map_split(info.mapping.desc_addr, info.mapping.avail_addr);
        used.map_split(info.mapping.used_addr);
        avail.valid = info.mapping.valid;
        used.valid = info.mapping.valid;
        avail.cur_avail_idx = Wrapping(info.avail_idx);
        used.used_idx = Wrapping(info.used_idx);
    }
    pub fn pop_avail(
        &self,
        chain: &mut Chain,
        mem: &MemCtx,
    ) -> Option<(u16, u32)> {
        assert!(chain.idx.is_none());
        let mut avail = self.avail.lock().unwrap();
        let req = avail.read_next_avail(self.size, mem)?;

        let mut desc = avail.read_ring_descr(req.desc_idx, self.size, mem)?;
        let mut flags = DescFlag::from_bits_truncate(desc.flags);
        let mut count = 0;
        let mut len = 0;
        chain.idx = Some(req.desc_idx);
        probes::virtio_vq_pop!(|| (
            self as *const VirtQueue as u64,
            req.desc_idx,
            req.avail_idx,
        ));

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
                    return Some((req.avail_idx, len));
                }
            } else {
                return Some((req.avail_idx, len));
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
        Some((req.avail_idx, len))
    }
    pub fn push_used(&self, chain: &mut Chain, mem: &MemCtx) {
        assert!(chain.idx.is_some());
        let mut used = self.used.lock().unwrap();
        let id = mem::replace(&mut chain.idx, None).unwrap();
        // XXX: for now, just go off of the write stats
        let len = chain.write_stat.bytes - chain.write_stat.bytes_remain;
        probes::virtio_vq_push!(|| (self as *const VirtQueue as u64, id, len));
        used.write_used(id, len, self.size, mem);
        if !used.intr_supressed(mem) {
            if let Some(intr) = used.interrupt.as_ref() {
                intr.notify();
            }
        }
        chain.reset();
    }

    /// Set the backing interrupt resource for VQ
    pub(super) fn set_intr(&self, intr: Box<dyn VirtioIntr>) {
        let mut used = self.used.lock().unwrap();
        used.interrupt = Some(intr)
    }

    /// Read the interrupt configuration for the `Used` ring
    pub(super) fn read_intr(&self) -> Option<VqIntr> {
        let used = self.used.lock().unwrap();
        used.interrupt.as_ref().map(|x| x.read())
    }

    /// Send an interrupt for VQ
    pub(super) fn send_intr(&self, mem: &MemCtx) {
        let used = self.used.lock().unwrap();
        if !used.intr_supressed(mem) {
            if let Some(intr) = used.interrupt.as_ref() {
                intr.notify();
            }
        }
    }

    pub fn export(&self) -> migrate::VirtQueueV1 {
        let avail = self.avail.lock().unwrap();
        let used = self.used.lock().unwrap();

        migrate::VirtQueueV1 {
            id: self.id,
            size: self.size,
            descr_gpa: avail.gpa_desc.0,
            mapping_valid: avail.valid && used.valid,
            live: self.live.load(Ordering::Acquire),

            // `flags` field is the first member for avail and used rings
            avail_gpa: avail.gpa_flags.0,
            used_gpa: used.gpa_flags.0,

            avail_cur_idx: avail.cur_avail_idx.0,
            used_idx: used.used_idx.0,
        }
    }

    pub fn import(
        &self,
        state: migrate::VirtQueueV1,
    ) -> Result<(), MigrateStateError> {
        let mut avail = self.avail.lock().unwrap();
        let mut used = self.used.lock().unwrap();

        if self.id != state.id {
            return Err(MigrateStateError::ImportFailed(format!(
                "VirtQueue: mismatched IDs {} vs {}",
                self.id, state.id,
            )));
        }
        if self.size != state.size {
            return Err(MigrateStateError::ImportFailed(format!(
                "VirtQueue: mismatched size {} vs {}",
                self.size, state.size,
            )));
        }

        avail.map_split(state.descr_gpa, state.avail_gpa);
        avail.valid = state.mapping_valid;
        avail.cur_avail_idx = Wrapping(state.avail_cur_idx);

        used.map_split(state.used_gpa);
        used.valid = state.mapping_valid;
        used.used_idx = Wrapping(state.used_idx);
        self.live.store(state.live, Ordering::Release);

        Ok(())
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
        let (stat, len) = match buf {
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
        // Safety: We assume the mutable item reference we have received is
        // valid (aligned, etc) to begin with.  It is cast into a u8 slice to
        // handle cases where it cannot be filled by a single buffer copy.
        let raw = unsafe {
            std::slice::from_raw_parts_mut(item as *mut T as *mut u8, item_sz)
        };
        let mut done = 0;
        let total = self.for_remaining_type(true, |addr, len| {
            let mut remain = GuestData::from(&mut raw[done..]);
            if let Some(copied) = mem.read_into(addr, &mut remain, len) {
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
    /// Fetch a string of readable guest regions from the chain, provided there
    /// are enough to cover a specified length.
    pub fn readable_bufs(&mut self, len: usize) -> Option<Vec<GuestRegion>> {
        if len == 0 || (self.read_stat.bytes_remain as usize) < len {
            return None;
        }

        let mut bufs = Vec::new();
        let mut remain = len;
        self.for_remaining_type(true, |addr, blen| {
            let to_consume = usize::min(blen, remain);

            bufs.push(GuestRegion(addr, to_consume));

            // Since we checked for enough remaining bytes ahead of time, there
            // should be no risk of this failing.
            remain = remain.checked_sub(to_consume).unwrap();
            (to_consume, remain != 0)
        });
        assert_eq!(remain, 0);
        Some(bufs)
    }
    pub fn write<T: Copy>(&mut self, item: &T, mem: &MemCtx) -> bool {
        let item_sz = mem::size_of::<T>();
        if (self.write_stat.bytes_remain as usize) < item_sz {
            return false;
        }
        // Safety: We assume the item reference we have received is valid
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
        let remain = len;
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
    /// Fetch a string of writable guest regions from the chain, provided there
    /// are enough to cover a specified length.
    pub fn writable_bufs(&mut self, len: usize) -> Option<Vec<GuestRegion>> {
        if len == 0 || (self.write_stat.bytes_remain as usize) < len {
            return None;
        }

        let mut bufs = Vec::new();
        let mut remain = len;
        self.for_remaining_type(false, |addr, blen| {
            let to_consume = usize::min(blen, remain);

            bufs.push(GuestRegion(addr, to_consume));

            // Since we checked for enough remaining bytes ahead of time, there
            // should be no risk of this failing.
            remain = remain.checked_sub(to_consume).unwrap();
            (to_consume, remain != 0)
        });
        assert_eq!(remain, 0);
        Some(bufs)
    }

    pub fn remain_write_bytes(&self) -> usize {
        self.write_stat.bytes_remain as usize
    }
    pub fn remain_read_bytes(&self) -> usize {
        self.read_stat.bytes_remain as usize
    }

    pub(crate) fn for_remaining_type<F>(
        &mut self,
        is_read: bool,
        mut f: F,
    ) -> usize
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
            let off_addr = GuestAddr(addr.0 + u64::from(stat.pos_off));
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

#[derive(Debug)]
pub struct MapInfo {
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
    pub valid: bool,
}
#[derive(Debug)]
pub struct Info {
    pub mapping: MapInfo,
    pub avail_idx: u16,
    pub used_idx: u16,
}

pub struct VirtQueues {
    queues: Vec<Arc<VirtQueue>>,
    num: NonZeroU16,
}
impl VirtQueues {
    pub fn new(
        size: NonZeroU16,
        num: NonZeroU16,
    ) -> Result<Self, VirtQueuesError> {
        let queues = (0..num.get())
            .map(|id| {
                if size.get().is_power_of_two() {
                    Ok(Arc::new(VirtQueue::new(id, size.get())))
                } else {
                    Err(VirtQueuesError::NonPowerOfTwoQueueSize(
                        usize::from(id),
                        usize::from(size.get()),
                    ))
                }
            })
            .collect::<Result<_, _>>()?;

        Ok(Self { queues, num })
    }
    pub fn new_from_sizes(
        sizes: &[NonZeroU16],
    ) -> Result<Self, VirtQueuesError> {
        let num = u16::try_from(sizes.len())
            .and_then(NonZeroU16::try_from)
            .map_err(|_| VirtQueuesError::BadQueueCount(sizes.len()))?;

        let queues = sizes
            .iter()
            .enumerate()
            .map(|(id, size)| {
                if size.get().is_power_of_two() {
                    Ok(Arc::new(VirtQueue::new(
                        u16::try_from(id).expect(
                            "proven to be drawn from a NonZeroU16 above",
                        ),
                        size.get(),
                    )))
                } else {
                    Err(VirtQueuesError::NonPowerOfTwoQueueSize(
                        id,
                        usize::from(size.get()),
                    ))
                }
            })
            .collect::<Result<_, _>>()?;

        Ok(Self { queues, num })
    }
    pub fn queue_size(&self, qid: u16) -> Option<u16> {
        self.get(qid).map(|v| v.size)
    }
    pub fn count(&self) -> NonZeroU16 {
        self.num
    }
    pub fn get(&self, qid: u16) -> Option<&Arc<VirtQueue>> {
        self.queues.get(usize::from(qid))
    }
    pub fn iter(&self) -> std::slice::Iter<'_, Arc<VirtQueue>> {
        self.queues.iter()
    }
}

impl<S: SliceIndex<[Arc<VirtQueue>]>> Index<S> for VirtQueues {
    type Output = S::Output;

    fn index(&self, index: S) -> &Self::Output {
        Index::index(&self.queues, index)
    }
}

#[derive(Copy, Clone, Debug, thiserror::Error)]
pub enum VirtQueuesError {
    #[error("queue {0}'s length ({1}) must be a power of two")]
    NonPowerOfTwoQueueSize(usize, usize),
    #[error("queue count {0} must be nonzero and less than 65535")]
    BadQueueCount(usize),
}

pub mod migrate {
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct VirtQueueV1 {
        pub id: u16,
        pub size: u16,
        pub descr_gpa: u64,
        pub mapping_valid: bool,
        pub live: bool,

        pub avail_gpa: u64,
        pub avail_cur_idx: u16,

        pub used_gpa: u64,
        pub used_idx: u16,
    }
}

#[cfg(feature = "falcon")]
pub(crate) fn write_buf(buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
    // more copy pasta from Chain::write b/c like Chain:read a
    // statically sized type is expected.
    let mut done = 0;
    let _total = chain.for_remaining_type(false, |addr, len| {
        let remain = &buf[done..];
        if let Some(copied) = mem.write_from(addr, remain, len) {
            let need_more = copied != remain.len();

            done += copied;
            (copied, need_more)
        } else {
            // Copy failed, so do not attempt anything else
            (0, false)
        }
    });
}
