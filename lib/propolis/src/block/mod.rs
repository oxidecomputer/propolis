// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implements an interface to virtualized block devices.

use std::time::Duration;

use crate::common::*;
use crate::vmm::{MemCtx, SubMapping};

mod file;
pub use file::FileBackend;

#[cfg(feature = "crucible")]
mod crucible;
#[cfg(feature = "crucible")]
pub use self::crucible::CrucibleBackend;

mod in_memory;
pub use in_memory::InMemoryBackend;

mod mem_async;
pub use mem_async::MemAsyncBackend;

mod noop;
pub use noop::NoopBackend;

mod noop_sync;
pub use noop_sync::NoopSyncBackend;

pub mod attachment;
pub mod minder;

pub use attachment::{
    attach, AsyncWorkerCtx, AttachError, BackendAttachment, DeviceAttachment,
    SyncWorkerCtx,
};
pub use minder::{DeviceQueue, DeviceRequest};

pub type ByteOffset = usize;
pub type ByteLen = usize;

/// When `block_size` is not specified in [BackendOpts], and the backend itself
/// is not choosing a block size, a default of 512B is used.
pub const DEFAULT_BLOCK_SIZE: u32 = 512;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn block_begin_read(devq_id: u64, req_id: u64, offset: u64, len: u64) {}
    fn block_begin_write(devq_id: u64, req_id: u64, offset: u64, len: u64) {}
    fn block_begin_flush(devq_id: u64, req_id: u64) {}
    fn block_begin_discard(devq_id: u64, req_id: u64, offset: u64, len: u64) {}

    fn block_complete_read(
        devq_id: u64,
        req_id: u64,
        result: u8,
        proc_ns: u64,
        queue_ns: u64,
    ) {
    }
    fn block_complete_write(
        devq_id: u64,
        req_id: u64,
        result: u8,
        proc_ns: u64,
        queue_ns: u64,
    ) {
    }
    fn block_complete_flush(
        devq_id: u64,
        req_id: u64,
        result: u8,
        proc_ns: u64,
        queue_ns: u64,
    ) {
    }
    fn block_complete_discard(
        devq_id: u64,
        req_id: u64,
        result: u8,
        proc_ns: u64,
        queue_ns: u64,
    ) {
    }

    fn block_completion_sent(devq_id: u64, req_id: u64, complete_ns: u64) {}

    fn block_poll(devq_id: u64, worker_id: u64, emit_req: u8) {}
    fn block_sleep(dev_id: u32, worker_id: u64) {}
    fn block_wake(dev_id: u32, worker_id: u64) {}
    fn block_notify(devq_id: u64) {}
    fn block_strategy(dev_id: u32, strat: String, generation: u64) {}

    fn block_worker_collection_wake(wake_wids: u64, limit: usize) {}
    fn block_worker_collection_woken(remaining_wids: u64, num_woken: usize) {}
}

/// Type of operations which may be issued to a virtual block device.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Operation {
    /// Read from `offset` for `len`
    Read(ByteOffset, ByteLen),
    /// Write to `offset` for len
    Write(ByteOffset, ByteLen),
    /// Flush buffer(s)
    Flush,
    /// Discard/UNMAP/deallocate region
    Discard(ByteOffset, ByteLen),
}
impl Operation {
    pub const fn is_read(&self) -> bool {
        matches!(self, Operation::Read(..))
    }
    pub const fn is_write(&self) -> bool {
        matches!(self, Operation::Write(..))
    }
    pub const fn is_flush(&self) -> bool {
        matches!(self, Operation::Flush)
    }
    pub const fn is_discard(&self) -> bool {
        matches!(self, Operation::Discard(..))
    }
}

/// Result of a block [`Request`]
#[derive(Copy, Clone, Debug)]
pub enum Result {
    /// Request succeeded
    Success = 0,
    /// Backend indicated failure for operation
    Failure,
    /// Underlying backend is read-only
    ReadOnly,
    /// Operation not supported by backend
    Unsupported,
}
impl Result {
    pub const fn is_err(&self) -> bool {
        !matches!(self, Result::Success)
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub struct QueueId(u8);
impl QueueId {
    /// Arbitrary limit for per-device queues.
    /// Sized to match [attachment::Bitmap] capacity
    pub const MAX_QUEUES: usize = 64;

    pub const MAX: Self = Self(Self::MAX_QUEUES as u8);

    /// Get the next sequential QueueId, wrapping around at a maximum
    fn next(self, max: usize) -> Self {
        let max: u8 = max.try_into().expect("max should be in-range");
        assert!(max != 0 && max <= Self::MAX.0);

        let next = self.0.wrapping_add(1);
        if next >= max {
            Self(0)
        } else {
            Self(next)
        }
    }
}
impl From<usize> for QueueId {
    fn from(value: usize) -> Self {
        assert!(value < Self::MAX_QUEUES);
        Self(value as u8)
    }
}
impl From<QueueId> for usize {
    fn from(value: QueueId) -> Self {
        value.0 as usize
    }
}
impl From<u16> for QueueId {
    fn from(value: u16) -> Self {
        assert!(value < (Self::MAX_QUEUES as u16));
        Self(value as u8)
    }
}
impl From<QueueId> for u16 {
    fn from(value: QueueId) -> Self {
        value.0 as u16
    }
}

pub type DeviceId = u32;
pub type WorkerId = usize;

/// Combine device and queue IDs into single u64 for probes
pub(crate) fn devq_id(dev: DeviceId, queue: QueueId) -> u64 {
    ((dev as u64) << 8) | (queue.0 as u64)
}

/// Block device operation request
#[derive(Clone)]
pub struct Request {
    /// The type of operation requested by the block device
    pub op: Operation,

    /// A list of regions of guest memory to read/write into as part of the I/O
    /// request
    pub regions: Vec<GuestRegion>,
}
impl Request {
    pub fn new_read(
        off: ByteOffset,
        len: ByteLen,
        regions: Vec<GuestRegion>,
    ) -> Self {
        Self { op: Operation::Read(off, len), regions }
    }

    pub fn new_write(
        off: ByteOffset,
        len: ByteLen,
        regions: Vec<GuestRegion>,
    ) -> Self {
        Self { op: Operation::Write(off, len), regions }
    }

    pub fn new_flush() -> Self {
        let op = Operation::Flush;
        Self { op, regions: Vec::new() }
    }

    pub fn new_discard(off: ByteOffset, len: ByteLen) -> Self {
        let op = Operation::Discard(off, len);
        Self { op, regions: Vec::new() }
    }

    pub fn mappings<'a>(&self, mem: &'a MemCtx) -> Option<Vec<SubMapping<'a>>> {
        match &self.op {
            Operation::Read(..) => {
                self.regions.iter().map(|r| mem.writable_region(r)).collect()
            }
            Operation::Write(..) => {
                self.regions.iter().map(|r| mem.readable_region(r)).collect()
            }
            Operation::Flush | Operation::Discard(..) => None,
        }
    }
}

/// Metadata regarding a virtualized block device.
#[derive(Default, Debug, Copy, Clone)]
pub struct DeviceInfo {
    /// Size (in bytes) per block
    pub block_size: u32,
    /// Device size in blocks (see above)
    pub total_size: u64,
    /// Is the device read-only
    pub read_only: bool,
    /// Does the device support discard/UNMAP
    pub supports_discard: bool,
}

/// Options to control behavior of block backend.
///
/// Values for omitted fields will be determined by the backend, likely by
/// querying the underlying resource.  If values provided conflict with said
/// resource, the backend may fail its initialization with an error.
#[derive(Default, Copy, Clone)]
pub struct BackendOpts {
    /// Size (in bytes) per block
    pub block_size: Option<u32>,

    /// Disallow writes (returning errors if attempted) and report a
    /// non-writable device (if frontend is capable)
    pub read_only: Option<bool>,

    /// Force flush requests to be skipped (turned into no-op)
    pub skip_flush: Option<bool>,
}

/// Top-level trait for block devices (frontends) to translate guest block IO
/// requests into [Request]s for the attached [Backend]
pub trait Device: Send + Sync + 'static {
    /// Access to the [DeviceAttachment] representing this device.
    fn attachment(&self) -> &DeviceAttachment;
}

/// Top-level trait for block backends which will attach to [Device]s in order
/// to process [Request]s posted by the guest.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Access to the [BackendAttachment] representing this backend.
    fn attachment(&self) -> &BackendAttachment;

    /// Start attempting to process [Request]s from [Device] (if attached)
    ///
    /// Spawning of any tasks required to do such request processing can be done
    /// as part of this start-up.
    ///
    /// This operation will be invoked only once per backend (when its VM
    /// starts). Block backends are not explicitly resumed during VM lifecycle
    /// events; instead, their corresponding devices will stop issuing new
    /// requests while paused and resume issuing them when they are resumed.
    ///
    /// WARNING: The caller may abort VM startup and cancel the future created
    /// by this routine. In this case the caller may not call [`Self::stop()`]
    /// prior to dropping the backend. This routine is, however, guaranteed to
    /// be called before the VM's vCPUs are started.
    ///
    async fn start(&self) -> anyhow::Result<()>;

    /// Stop attempting to process new [Request]s from [Device] (if attached)
    ///
    /// Any in-flight processing of requests should be concluded before this
    /// call returns.
    ///
    /// If any tasks were spawned as part of [Backend::start()], they should be
    /// brought to rest as part of this call.
    ///
    /// This operation will be invoked only once per backend (when its VM
    /// stops). Block backends are not explicitly paused during VM lifecycle
    /// events; instead, their corresponding devices will stop issuing new
    /// requests when they are told to pause (and will only report they are
    /// fully paused when all their in-flight requests have completed).
    async fn stop(&self);
}

/// Consumer of per-[Request] metrics
pub trait MetricConsumer: Send + Sync + 'static {
    /// Called upon the completion of each block [Request] when a MetricConsumer
    /// has been set for a given [DeviceAttachment].
    fn request_completed(
        &self,
        queue_id: QueueId,
        op: Operation,
        result: Result,
        time_queued: Duration,
        time_processed: Duration,
    );
}
