// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Test utilities for constructing fake virtqueues backed by real guest memory.
//!
//! This module provides [`TestVirtQueue`] for single-queue tests and
//! [`TestVirtQueues`] for multi-queue devices. Both allocate guest memory via
//! a tempfile-backed [`PhysMap`], lay out virtio ring structures, and provide
//! helpers to enqueue descriptor chains — simulating a guest driver writing
//! to the available ring.

use std::sync::Arc;

use zerocopy::FromBytes;

use crate::accessors::MemAccessor;
use crate::common::GuestAddr;
use crate::vmm::mem::PhysMap;
use crate::vmm::MemCtx;

// Re-export queue types so tests outside this module can access them
// without requiring `queue` to be pub(crate).
pub use super::queue::{Chain, DescFlag, VirtQueue, VirtQueues, VqSize};

/// Page size for alignment (4 KiB).
const PAGE_SIZE: u64 = 0x1000;

/// Size in bytes of a virtio descriptor (addr: u64, len: u32, flags: u16, next: u16).
const DESC_SIZE: u64 = 16;

/// Size in bytes of a used ring element (id: u32, len: u32).
const USED_ELEM_SIZE: u64 = 8;

/// Size in bytes of an available ring entry (descriptor index: u16).
const AVAIL_ELEM_SIZE: u64 = 2;

/// Size in bytes of the ring header (flags: u16, idx: u16).
const RING_HEADER_SIZE: u64 = 4;

/// Number of pages to allocate for the data area in tests.
const DATA_AREA_PAGES: u64 = 64;

/// Align `val` up to the next multiple of `align` (must be power of 2).
pub const fn align_up(val: u64, align: u64) -> u64 {
    (val + align - 1) & !(align - 1)
}

/// 16-byte virtio descriptor, matching the on-wire/in-memory layout.
#[repr(C)]
#[derive(Copy, Clone, Default, FromBytes)]
pub struct RawDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

/// 8-byte used ring element.
#[repr(C)]
#[derive(Copy, Clone, Default, FromBytes)]
pub struct RawUsedElem {
    pub id: u32,
    pub len: u32,
}

/// Guest physical address layout for a single virtqueue's ring structures.
#[derive(Copy, Clone, Debug)]
pub struct QueueLayout {
    pub desc_base: u64,
    pub avail_base: u64,
    pub used_base: u64,
    /// First GPA after this queue's structures.
    pub end: u64,
}

impl QueueLayout {
    /// Compute the ring layout for a queue of `size` entries starting at
    /// `base`.
    ///
    /// Layout follows the virtio 1.0 split virtqueue format:
    /// - Descriptor table: `size * DESC_SIZE` bytes
    /// - Available ring: header (4 bytes) + `size * 2` bytes for entries
    /// - Used ring: page-aligned, header (4 bytes) + `size * 8` bytes
    pub fn new(base: u64, size: u16) -> Self {
        let qsz = size as u64;
        let desc_base = base;
        let avail_base = desc_base + DESC_SIZE * qsz;
        let used_base = align_up(
            avail_base + RING_HEADER_SIZE + AVAIL_ELEM_SIZE * qsz,
            PAGE_SIZE,
        );
        let end = align_up(
            used_base + RING_HEADER_SIZE + USED_ELEM_SIZE * qsz,
            PAGE_SIZE,
        );
        Self { desc_base, avail_base, used_base, end }
    }
}

/// Per-queue writer for injecting descriptors into a virtqueue's rings.
pub struct QueueWriter {
    layout: QueueLayout,
    size: u16,
    /// Next free descriptor index.
    next_desc: u16,
    /// Start of data area for this queue.
    data_start: u64,
    /// Next free data area offset (GPA).
    data_cursor: u64,
    /// Avail ring index we've published up to.
    avail_idx: u16,
}

impl QueueWriter {
    /// Create a new QueueWriter for a queue with the given layout.
    pub fn new(layout: QueueLayout, size: u16, data_start: u64) -> Self {
        Self {
            layout,
            size,
            next_desc: 0,
            data_start,
            data_cursor: data_start,
            avail_idx: 0,
        }
    }

    /// Reset descriptor and data cursors to allow reusing slots.
    pub fn reset_cursors(&mut self) {
        self.next_desc = 0;
        self.data_cursor = self.data_start;
    }

    /// Write a descriptor and return its index.
    pub fn write_desc(
        &mut self,
        mem_acc: &MemAccessor,
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) -> u16 {
        let idx = self.next_desc;
        assert!(idx < self.size, "descriptor table exhausted");
        self.next_desc += 1;

        let desc = RawDesc { addr, len, flags, next };
        let gpa = self.layout.desc_base + u64::from(idx) * DESC_SIZE;
        let mem = mem_acc.access().unwrap();
        mem.write(GuestAddr(gpa), &desc);
        idx
    }

    /// Allocate data space and write bytes into it. Returns the GPA.
    pub fn write_data(&mut self, mem_acc: &MemAccessor, data: &[u8]) -> u64 {
        let gpa = self.data_cursor;
        self.data_cursor += data.len() as u64;
        let mem = mem_acc.access().unwrap();
        mem.write_from(GuestAddr(gpa), data, data.len());
        gpa
    }

    /// Allocate data space without writing. Returns the GPA.
    pub fn alloc_data(&mut self, len: u32) -> u64 {
        let gpa = self.data_cursor;
        self.data_cursor += u64::from(len);
        gpa
    }

    /// Add a readable descriptor with the given data.
    pub fn add_readable(&mut self, mem_acc: &MemAccessor, data: &[u8]) -> u16 {
        let gpa = self.write_data(mem_acc, data);
        self.write_desc(mem_acc, gpa, data.len() as u32, 0, 0)
    }

    /// Add a writable descriptor of the given size.
    pub fn add_writable(&mut self, mem_acc: &MemAccessor, len: u32) -> u16 {
        let gpa = self.alloc_data(len);
        self.write_desc(mem_acc, gpa, len, DescFlag::WRITE.bits(), 0)
    }

    /// Chain two descriptors together via NEXT flag.
    pub fn chain(&self, mem_acc: &MemAccessor, from: u16, to: u16) {
        let gpa = self.layout.desc_base + u64::from(from) * DESC_SIZE;
        let mem = mem_acc.access().unwrap();
        let mut raw: RawDesc = *mem.read(GuestAddr(gpa)).unwrap();
        raw.flags |= DescFlag::NEXT.bits();
        raw.next = to;
        mem.write(GuestAddr(gpa), &raw);
    }

    /// Publish a descriptor chain head on the available ring.
    pub fn publish_avail(&mut self, mem_acc: &MemAccessor, head: u16) {
        // Available ring layout:
        // flags (u16) | idx (u16) | ring[size] (u16 each)
        let slot = self.layout.avail_base
            + RING_HEADER_SIZE
            + u64::from(self.avail_idx % self.size) * AVAIL_ELEM_SIZE;
        self.avail_idx += 1;
        let new_idx = self.avail_idx;
        let mem = mem_acc.access().unwrap();
        mem.write(GuestAddr(slot), &head);
        // Write new index at offset 2 (after flags u16)
        mem.write(GuestAddr(self.layout.avail_base + 2), &new_idx);
    }

    /// Read the used ring index.
    pub fn used_idx(&self, mem_acc: &MemAccessor) -> u16 {
        let mem = mem_acc.access().unwrap();
        // Used ring idx is at offset 2 (after flags u16)
        *mem.read(GuestAddr(self.layout.used_base + 2)).unwrap()
    }

    /// Read a used ring entry by index, returning (desc_id, len).
    pub fn read_used_elem(
        &self,
        mem_acc: &MemAccessor,
        used_index: u16,
    ) -> RawUsedElem {
        let mem = mem_acc.access().unwrap();
        // Used ring layout:
        // flags (u16) | idx (u16) | ring[size] (RawUsedElem each)
        let entry_gpa = self.layout.used_base
            + RING_HEADER_SIZE
            + u64::from(used_index % self.size) * USED_ELEM_SIZE;
        *mem.read(GuestAddr(entry_gpa)).unwrap()
    }

    /// Read raw bytes from the buffer of a descriptor.
    pub fn read_desc_data(
        &self,
        mem_acc: &MemAccessor,
        desc_id: u16,
        len: usize,
    ) -> Vec<u8> {
        let mem = mem_acc.access().unwrap();
        let desc_gpa = self.layout.desc_base + u64::from(desc_id) * DESC_SIZE;
        let raw_desc: RawDesc = *mem.read(GuestAddr(desc_gpa)).unwrap();

        let mut data = vec![0u8; len];
        mem.read_into(
            GuestAddr(raw_desc.addr),
            &mut crate::common::GuestData::from(data.as_mut_slice()),
            len,
        );
        data
    }
}

/// Multi-queue test harness for virtio devices that use multiple queues.
pub struct TestVirtQueues {
    /// Must stay alive to keep memory mappings valid.
    _phys: PhysMap,
    mem_acc: MemAccessor,
    queues: VirtQueues,
    layouts: Vec<QueueLayout>,
    sizes: Vec<u16>,
    /// Start of data area (after all queue structures).
    data_start: u64,
}

impl TestVirtQueues {
    /// Create a new multi-queue test harness.
    ///
    /// `sizes` specifies the size of each queue (must be powers of 2).
    pub fn new(sizes: &[VqSize]) -> Self {
        // Compute layouts for all queues sequentially
        let mut layouts = Vec::with_capacity(sizes.len());
        let mut size_vals = Vec::with_capacity(sizes.len());
        let mut offset = 0u64;
        for &size in sizes {
            let size_u16: u16 = size.into();
            let layout = QueueLayout::new(offset, size_u16);
            offset = layout.end;
            layouts.push(layout);
            size_vals.push(size_u16);
        }

        // Data area after all rings
        let data_start = offset;
        let data_area_size = PAGE_SIZE * DATA_AREA_PAGES;
        let total_size =
            align_up(data_start + data_area_size, PAGE_SIZE) as usize;

        let mut phys = PhysMap::new_test(total_size);
        phys.add_test_mem("test-vqs".to_string(), 0, total_size)
            .expect("add test mem");
        let mem_acc = phys.finalize();

        // Create VirtQueues
        let queues = VirtQueues::new(sizes);

        // Initialize each queue
        for (i, layout) in layouts.iter().enumerate() {
            let vq = queues.get(i as u16).unwrap();
            mem_acc.adopt(&vq.acc_mem, Some(format!("test-vq-{i}")));
            vq.map_virtqueue(
                layout.desc_base,
                layout.avail_base,
                layout.used_base,
            );
            vq.live.store(true, std::sync::atomic::Ordering::Release);
            vq.enabled.store(true, std::sync::atomic::Ordering::Release);

            // Zero out avail and used ring headers
            let mem = mem_acc.access().unwrap();
            mem.write(GuestAddr(layout.avail_base), &0u16);
            mem.write(GuestAddr(layout.avail_base + 2), &0u16);
            mem.write(GuestAddr(layout.used_base), &0u16);
            mem.write(GuestAddr(layout.used_base + 2), &0u16);
        }

        Self {
            _phys: phys,
            mem_acc,
            queues,
            layouts,
            sizes: size_vals,
            data_start,
        }
    }

    /// Get the memory accessor.
    pub fn mem_acc(&self) -> &MemAccessor {
        &self.mem_acc
    }

    /// Get the underlying VirtQueues.
    pub fn queues(&self) -> &VirtQueues {
        &self.queues
    }

    /// Get the VirtQueue at the given index.
    pub fn vq(&self, idx: u16) -> &Arc<VirtQueue> {
        self.queues.get(idx).unwrap()
    }

    /// Create a QueueWriter for the given queue index.
    ///
    /// `data_offset` is an offset from the shared data area start,
    /// allowing different queues to use different regions.
    pub fn writer(&self, queue_idx: usize, data_offset: u64) -> QueueWriter {
        let layout = self.layouts[queue_idx];
        let size = self.sizes[queue_idx];
        QueueWriter::new(layout, size, self.data_start + data_offset)
    }

    /// Get the layout for a queue.
    pub fn layout(&self, queue_idx: usize) -> QueueLayout {
        self.layouts[queue_idx]
    }
}

/// A test harness wrapping guest memory and a single virtqueue.
///
/// For multi-queue tests, use [`TestVirtQueues`] instead.
pub struct TestVirtQueue {
    inner: TestVirtQueues,
    writer: QueueWriter,
}

impl TestVirtQueue {
    /// Create a new test virtqueue.
    ///
    /// `queue_size` must be a power of 2.
    pub fn new(queue_size: u16) -> Self {
        let inner = TestVirtQueues::new(&[VqSize::new(queue_size)]);
        let writer = inner.writer(0, 0);
        Self { inner, writer }
    }

    /// Get the underlying `VirtQueue`.
    pub fn vq(&self) -> &Arc<VirtQueue> {
        self.inner.vq(0)
    }

    /// Get a `MemCtx` guard for directly reading/writing guest memory.
    pub fn mem(&self) -> impl std::ops::Deref<Target = MemCtx> + '_ {
        self.inner.mem_acc().access().expect("test mem accessible")
    }

    /// Add a readable descriptor containing `data`.
    ///
    /// Returns the descriptor index.
    pub fn add_readable(&mut self, data: &[u8]) -> u16 {
        self.writer.add_readable(self.inner.mem_acc(), data)
    }

    /// Add a writable descriptor of `len` bytes.
    ///
    /// Returns the descriptor index.
    pub fn add_writable(&mut self, len: u32) -> u16 {
        self.writer.add_writable(self.inner.mem_acc(), len)
    }

    /// Link descriptors into a chain by setting NEXT flags.
    ///
    /// `descs` should be in order: `[head, ..., tail]`.
    pub fn chain_descriptors(&mut self, descs: &[u16]) {
        for i in 0..descs.len().saturating_sub(1) {
            self.writer.chain(self.inner.mem_acc(), descs[i], descs[i + 1]);
        }
    }

    /// Publish a descriptor chain head on the available ring.
    pub fn publish_avail(&mut self, head: u16) {
        self.writer.publish_avail(self.inner.mem_acc(), head);
    }

    /// Read all entries from the used ring.
    ///
    /// Returns `(descriptor_id, bytes_written)` pairs.
    pub fn read_used(&self) -> Vec<(u32, u32)> {
        let used_idx = self.writer.used_idx(self.inner.mem_acc());
        (0..used_idx)
            .map(|i| {
                let elem = self.writer.read_used_elem(self.inner.mem_acc(), i);
                (elem.id, elem.len)
            })
            .collect()
    }

    /// Pop a chain from the available ring and return it.
    pub fn pop_chain(&self) -> Option<(Chain, u16, u32)> {
        let mem = self.inner.mem_acc().access()?;
        let mut chain = Chain::with_capacity(64);
        let (avail_idx, len) = self.vq().pop_avail(&mut chain, &mem)?;
        Some((chain, avail_idx, len))
    }

    /// Push a chain back to the used ring.
    pub fn push_used(&self, chain: &mut Chain) {
        let mem = self.inner.mem_acc().access().unwrap();
        self.vq().push_used(chain, &mem);
    }

    /// Get the GPA of a descriptor's buffer.
    pub fn desc_addr(&self, idx: u16) -> u64 {
        let mem = self.inner.mem_acc().access().unwrap();
        let desc_gpa =
            self.inner.layout(0).desc_base + u64::from(idx) * DESC_SIZE;
        let raw: RawDesc = *mem.read(GuestAddr(desc_gpa)).unwrap();
        raw.addr
    }

    /// Read raw bytes from guest memory at a given GPA.
    pub fn read_guest_mem(&self, addr: u64, len: usize) -> Vec<u8> {
        let mem = self.inner.mem_acc().access().unwrap();
        let mut buf = vec![0u8; len];
        let mut guest_buf = crate::common::GuestData::from(buf.as_mut_slice());
        mem.read_into(GuestAddr(addr), &mut guest_buf, len);
        buf
    }
}

#[cfg(test)]
mod test {
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

    #[test]
    fn multi_queue_smoke() {
        let tvqs = TestVirtQueues::new(&[
            VqSize::new(64),
            VqSize::new(64),
            VqSize::new(1),
        ]);

        let mut writer0 = tvqs.writer(0, 0);
        let mut writer1 = tvqs.writer(1, PAGE_SIZE);

        let d0 = writer0.add_readable(tvqs.mem_acc(), b"queue0");
        writer0.publish_avail(tvqs.mem_acc(), d0);

        let d1 = writer1.add_readable(tvqs.mem_acc(), b"queue1");
        writer1.publish_avail(tvqs.mem_acc(), d1);

        // Pop from each queue
        let mem = tvqs.mem_acc().access().unwrap();
        let mut chain0 = Chain::with_capacity(64);
        let mut chain1 = Chain::with_capacity(64);

        assert!(tvqs.vq(0).pop_avail(&mut chain0, &mem).is_some());
        assert!(tvqs.vq(1).pop_avail(&mut chain1, &mem).is_some());

        assert_eq!(chain0.remain_read_bytes(), 6);
        assert_eq!(chain1.remain_read_bytes(), 6);
    }

    #[test]
    fn queue_writer_reset_cursors() {
        let tvqs = TestVirtQueues::new(&[VqSize::new(16)]);
        let mut writer = tvqs.writer(0, 0);

        // Add some descriptors
        let d0 = writer.add_readable(tvqs.mem_acc(), b"first");
        writer.publish_avail(tvqs.mem_acc(), d0);

        // Reset and reuse
        writer.reset_cursors();

        let d1 = writer.add_readable(tvqs.mem_acc(), b"second");
        assert_eq!(d1, 0, "descriptor index should reset to 0");
        writer.publish_avail(tvqs.mem_acc(), d1);

        // Both publishes should have worked
        assert_eq!(writer.used_idx(tvqs.mem_acc()), 0); // Nothing consumed yet
    }
}
