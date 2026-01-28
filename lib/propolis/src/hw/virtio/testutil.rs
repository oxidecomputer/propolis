// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Test utilities for constructing fake virtqueues backed by real guest memory.
//!
//! This module provides [`TestVirtQueue`], a harness that allocates guest memory
//! via a tempfile-backed [`PhysMap`], lays out virtio ring structures, and
//! provides helpers to enqueue descriptor chains — simulating a guest driver
//! writing to the available ring.

use std::sync::Arc;

use zerocopy::FromBytes;

use crate::accessors::MemAccessor;
use crate::common::GuestAddr;
use crate::vmm::mem::PhysMap;
use crate::vmm::MemCtx;

// Re-export queue types so tests outside this module can access them
// without requiring `queue` to be pub(crate).
pub(crate) use super::queue::{Chain, DescFlag, VirtQueue, VirtQueues, VqSize};

/// 16-byte virtio descriptor, matching the on-wire/in-memory layout.
#[repr(C)]
#[derive(Copy, Clone, Default, FromBytes)]
struct RawDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// 8-byte used ring element.
#[repr(C)]
#[derive(Copy, Clone, Default, FromBytes)]
struct RawUsedElem {
    id: u32,
    len: u32,
}

const PAGE_SIZE: u64 = 0x1000;

/// Align `val` up to the next multiple of `align` (must be power of 2).
const fn align_up(val: u64, align: u64) -> u64 {
    (val + align - 1) & !(align - 1)
}

/// A test harness wrapping guest memory and virtqueue ring structures.
///
/// Provides methods to build descriptor chains in guest memory and publish
/// them on the available ring, simulating a guest driver. After the device
/// under test processes the chain, [`read_used`] can inspect the used ring.
pub(crate) struct TestVirtQueue {
    /// Must stay alive to keep memory mappings valid.
    _phys: PhysMap,
    /// Root memory accessor — the VirtQueue's acc_mem is adopted under this.
    mem_acc: MemAccessor,
    /// All queues (we use index 0).
    queues: VirtQueues,
    /// Guest physical address of descriptor table.
    desc_base: u64,
    /// Guest physical address of avail ring (flags field).
    avail_base: u64,
    /// Guest physical address of used ring (flags field).
    used_base: u64,
    /// Queue size.
    size: u16,
    /// Next descriptor slot to allocate.
    next_desc: u16,
    /// Next region of the data area to use for payload buffers.
    data_cursor: u64,
    /// End of the data area.
    data_end: u64,
    /// Current avail index we've published.
    avail_idx: u16,
}

impl TestVirtQueue {
    /// Create a new test virtqueue.
    ///
    /// `queue_size` must be a power of 2.
    pub fn new(queue_size: u16) -> Self {
        let qsz = queue_size as u64;

        // Compute layout sizes
        let desc_table_size = 16 * qsz;
        let avail_ring_size = 4 + 2 * qsz;
        let used_ring_size = 4 + 8 * qsz;

        let desc_offset: u64 = 0;
        let avail_offset = desc_offset + desc_table_size;
        let used_offset =
            align_up(avail_offset + avail_ring_size, PAGE_SIZE);
        let data_offset =
            align_up(used_offset + used_ring_size, PAGE_SIZE);

        // Allocate enough for rings + a generous data area
        let data_area_size = PAGE_SIZE * 4;
        let total_size =
            align_up(data_offset + data_area_size, PAGE_SIZE) as usize;

        let mut phys = PhysMap::new_test(total_size);
        phys.add_test_mem("test-vq".to_string(), 0, total_size)
            .expect("add test mem");
        let mem_acc = phys.finalize();

        // Create a VirtQueues with a single queue
        let vq_size = VqSize::new(queue_size);
        let queues = VirtQueues::new(&[vq_size]);
        let vq = queues.get(0).unwrap();

        // Adopt the VirtQueue's orphaned MemAccessor under our root
        mem_acc.adopt(&vq.acc_mem, Some("test-vq-acc".to_string()));

        // Map the virtqueue to our layout
        vq.map_virtqueue(desc_offset, avail_offset, used_offset);
        vq.live
            .store(true, std::sync::atomic::Ordering::Release);
        vq.enabled
            .store(true, std::sync::atomic::Ordering::Release);

        // Initialize avail ring: flags=0, idx=0
        {
            let mem = mem_acc.access().unwrap();
            mem.write(GuestAddr(avail_offset), &0u16);
            mem.write(GuestAddr(avail_offset + 2), &0u16);
            mem.write(GuestAddr(used_offset), &0u16);
            mem.write(GuestAddr(used_offset + 2), &0u16);
        }

        Self {
            _phys: phys,
            mem_acc,
            queues,
            desc_base: desc_offset,
            avail_base: avail_offset,
            used_base: used_offset,
            size: queue_size,
            next_desc: 0,
            data_cursor: data_offset,
            data_end: data_offset + data_area_size,
            avail_idx: 0,
        }
    }

    /// Get the underlying `VirtQueue`.
    pub fn vq(&self) -> &Arc<VirtQueue> {
        self.queues.get(0).unwrap()
    }

    /// Get a `MemCtx` guard for directly reading/writing guest memory.
    pub fn mem(&self) -> impl std::ops::Deref<Target = MemCtx> + '_ {
        self.mem_acc.access().expect("test mem should be accessible")
    }

    /// Return a child `MemAccessor` suitable for wiring into other components
    /// (e.g. `VsockVq`).
    pub fn mem_accessor(&self) -> MemAccessor {
        self.mem_acc.child(Some("test-child".to_string()))
    }

    /// Allocate a region from the data area and return its GPA.
    fn alloc_data(&mut self, len: u32) -> u64 {
        let addr = self.data_cursor;
        self.data_cursor += u64::from(len);
        assert!(
            self.data_cursor <= self.data_end,
            "test data area exhausted"
        );
        addr
    }

    /// Allocate a descriptor slot and write it into the descriptor table.
    fn alloc_desc(
        &mut self,
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) -> u16 {
        let idx = self.next_desc;
        assert!(idx < self.size, "descriptor table exhausted");
        self.next_desc += 1;

        let desc = RawDesc { addr, len, flags, next };
        let desc_addr = self.desc_base + u64::from(idx) * 16;
        let mem = self.mem();
        mem.write(GuestAddr(desc_addr), &desc);

        idx
    }

    /// Add a readable descriptor containing `data`.
    ///
    /// Returns the descriptor index.
    pub fn add_readable(&mut self, data: &[u8]) -> u16 {
        let len = data.len() as u32;
        let gpa = self.alloc_data(len);

        let mem = self.mem();
        mem.write_from(GuestAddr(gpa), data, data.len());
        drop(mem);

        self.alloc_desc(gpa, len, 0, 0)
    }

    /// Add a writable descriptor of `len` bytes.
    ///
    /// Returns the descriptor index.
    pub fn add_writable(&mut self, len: u32) -> u16 {
        let gpa = self.alloc_data(len);
        self.alloc_desc(gpa, len, DescFlag::WRITE.bits(), 0)
    }

    /// Link descriptors into a chain by setting NEXT flags.
    ///
    /// `descs` should be in order: `[head, ..., tail]`.
    pub fn chain_descriptors(&self, descs: &[u16]) {
        if descs.len() <= 1 {
            return;
        }
        let mem = self.mem();
        for i in 0..descs.len() - 1 {
            let desc_addr = self.desc_base + u64::from(descs[i]) * 16;
            let mut raw: RawDesc =
                *mem.read(GuestAddr(desc_addr)).unwrap();
            raw.flags |= DescFlag::NEXT.bits();
            raw.next = descs[i + 1];
            mem.write(GuestAddr(desc_addr), &raw);
        }
    }

    /// Publish a descriptor chain head on the available ring.
    pub fn publish_avail(&mut self, head: u16) {
        let ring_entry_addr = self.avail_base
            + 4
            + u64::from(self.avail_idx % self.size) * 2;
        self.avail_idx += 1;
        let new_idx = self.avail_idx;

        let mem = self.mem();
        mem.write(GuestAddr(ring_entry_addr), &head);
        mem.write(GuestAddr(self.avail_base + 2), &new_idx);
    }

    /// Read all entries from the used ring.
    ///
    /// Returns `(descriptor_id, bytes_written)` pairs.
    pub fn read_used(&self) -> Vec<(u32, u32)> {
        let mem = self.mem();
        let used_idx: u16 =
            *mem.read(GuestAddr(self.used_base + 2)).unwrap();

        let mut entries = Vec::new();
        for i in 0..used_idx {
            let entry_addr =
                self.used_base + 4 + u64::from(i % self.size) * 8;
            let elem: RawUsedElem =
                *mem.read(GuestAddr(entry_addr)).unwrap();
            entries.push((elem.id, elem.len));
        }
        entries
    }

    /// Read raw bytes from guest memory at a given GPA.
    pub fn read_guest_mem(&self, addr: u64, len: usize) -> Vec<u8> {
        let mem = self.mem();
        let mut buf = vec![0u8; len];
        let mut guest_buf =
            crate::common::GuestData::from(buf.as_mut_slice());
        mem.read_into(GuestAddr(addr), &mut guest_buf, len);
        buf
    }

    /// Get the GPA of a descriptor's buffer.
    pub fn desc_addr(&self, idx: u16) -> u64 {
        let mem = self.mem();
        let desc_gpa = self.desc_base + u64::from(idx) * 16;
        let raw: RawDesc = *mem.read(GuestAddr(desc_gpa)).unwrap();
        raw.addr
    }

    /// Pop a chain from the available ring and return it.
    pub fn pop_chain(&self) -> Option<(Chain, u16, u32)> {
        let mem = self.mem();
        let mut chain = Chain::with_capacity(self.size as usize);
        let (avail_idx, len) = self.vq().pop_avail(&mut chain, &mem)?;
        Some((chain, avail_idx, len))
    }

    /// Push a chain back to the used ring.
    pub fn push_used(&self, chain: &mut Chain) {
        let mem = self.mem();
        self.vq().push_used(chain, &mem);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_pop_avail_readable() {
        let mut tvq = TestVirtQueue::new(16);

        let data = b"hello virtqueue";
        let d0 = tvq.add_readable(data);
        tvq.publish_avail(d0);

        let (mut chain, _avail_idx, total_len) = tvq.pop_chain().unwrap();
        assert_eq!(total_len, data.len() as u32);

        let mem = tvq.mem();
        let mut buf = [0u8; 15];
        assert!(chain.read(&mut buf, &mem));
        assert_eq!(&buf, data);
    }

    #[test]
    fn smoke_pop_avail_writable() {
        let mut tvq = TestVirtQueue::new(16);

        let d0 = tvq.add_writable(64);
        tvq.publish_avail(d0);

        let (mut chain, _avail_idx, total_len) = tvq.pop_chain().unwrap();
        assert_eq!(total_len, 64);

        let mem = tvq.mem();
        let payload = b"written by device";
        assert!(chain.write(payload, &mem));
        drop(mem);

        tvq.push_used(&mut chain);

        let used = tvq.read_used();
        assert_eq!(used.len(), 1);
        assert_eq!(used[0].0, d0 as u32);
        assert_eq!(used[0].1, payload.len() as u32);

        let addr = tvq.desc_addr(d0);
        let read_back = tvq.read_guest_mem(addr, payload.len());
        assert_eq!(read_back, payload);
    }

    #[test]
    fn smoke_chained_descriptors() {
        let mut tvq = TestVirtQueue::new(16);

        let header_data = [0xAA; 8];
        let body_data = [0xBB; 32];
        let d0 = tvq.add_readable(&header_data);
        let d1 = tvq.add_readable(&body_data);
        tvq.chain_descriptors(&[d0, d1]);
        tvq.publish_avail(d0);

        let (mut chain, _avail_idx, total_len) = tvq.pop_chain().unwrap();
        assert_eq!(total_len, 40);

        let mem = tvq.mem();
        let mut hdr = [0u8; 8];
        assert!(chain.read(&mut hdr, &mem));
        assert_eq!(hdr, header_data);

        let mut body = [0u8; 32];
        assert!(chain.read(&mut body, &mem));
        assert_eq!(body, body_data);
    }

    #[test]
    fn smoke_mixed_chain() {
        let mut tvq = TestVirtQueue::new(16);

        let req_data = [0x01, 0x02, 0x03, 0x04];
        let d0 = tvq.add_readable(&req_data);
        let d1 = tvq.add_writable(128);
        tvq.chain_descriptors(&[d0, d1]);
        tvq.publish_avail(d0);

        let (mut chain, _, total_len) = tvq.pop_chain().unwrap();
        assert_eq!(total_len, 4 + 128);

        let mem = tvq.mem();

        let mut req = [0u8; 4];
        assert!(chain.read(&mut req, &mem));
        assert_eq!(req, req_data);

        let resp = [0xFF; 16];
        assert!(chain.write(&resp, &mem));
        drop(mem);

        tvq.push_used(&mut chain);

        let addr = tvq.desc_addr(d1);
        let read_back = tvq.read_guest_mem(addr, 16);
        assert_eq!(read_back, &resp);
    }

    #[test]
    fn empty_avail_ring_returns_none() {
        let tvq = TestVirtQueue::new(16);
        assert!(tvq.pop_chain().is_none());
    }

    #[test]
    fn multiple_chains() {
        let mut tvq = TestVirtQueue::new(16);

        let d0 = tvq.add_readable(b"first");
        tvq.publish_avail(d0);

        let d1 = tvq.add_readable(b"second");
        tvq.publish_avail(d1);

        let (chain0, _, _) = tvq.pop_chain().unwrap();
        let (chain1, _, _) = tvq.pop_chain().unwrap();
        assert!(tvq.pop_chain().is_none());

        assert_ne!(chain0.remain_read_bytes(), chain1.remain_read_bytes());
    }
}
