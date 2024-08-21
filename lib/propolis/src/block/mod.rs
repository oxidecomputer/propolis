// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implements an interface to virtualized block devices.

use std::borrow::Borrow;

use crate::accessors::MemAccessor;
use crate::attachment::DetachError;
use crate::common::*;
use crate::vmm::{MemCtx, SubMapping};

mod file;
pub use file::FileBackend;
use tracking::CompletionCallback;

#[cfg(feature = "crucible")]
mod crucible;
#[cfg(feature = "crucible")]
pub use self::crucible::CrucibleBackend;

mod in_memory;
pub use in_memory::InMemoryBackend;

mod mem_async;
pub use mem_async::MemAsyncBackend;

pub mod attachment;
pub mod tracking;

pub use attachment::{attach, BackendAttachment, DeviceAttachment};

pub type ByteOffset = usize;
pub type ByteLen = usize;

/// When `block_size` is not specified in [BackendOpts], and the backend itself
/// is not choosing a block size, a default of 512B is used.
pub const DEFAULT_BLOCK_SIZE: u32 = 512;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn block_begin_read(dev_id: u64, req_id: u64, offset: u64, len: u64) {}
    fn block_begin_write(dev_id: u64, req_id: u64, offset: u64, len: u64) {}
    fn block_begin_flush(dev_id: u64, req_id: u64) {}

    fn block_complete_read(
        dev_id: u64,
        req_id: u64,
        result: u8,
        proc_ns: u64,
        queue_ns: u64,
    ) {
    }
    fn block_complete_write(
        dev_id: u64,
        req_id: u64,
        result: u8,
        proc_ns: u64,
        queue_ns: u64,
    ) {
    }
    fn block_complete_flush(
        dev_id: u64,
        req_id: u64,
        result: u8,
        proc_ns: u64,
        queue_ns: u64,
    ) {
    }
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

/// Block device operation request
pub struct Request {
    /// The type of operation requested by the block device
    op: Operation,

    /// A list of regions of guest memory to read/write into as part of the I/O
    /// request
    regions: Vec<GuestRegion>,

    /// Store [`tracking::TrackingMarker`] when this request is tracked by a
    /// [`tracking::Tracking`] for that device.  It is through this marker that
    /// the result of the block request is communicated back to the device
    /// emulation for processing.
    marker: Option<tracking::TrackingMarker>,
}
impl Request {
    pub fn new_read(
        off: ByteOffset,
        len: ByteLen,
        regions: Vec<GuestRegion>,
    ) -> Self {
        Self { op: Operation::Read(off, len), regions, marker: None }
    }

    pub fn new_write(
        off: ByteOffset,
        len: ByteLen,
        regions: Vec<GuestRegion>,
    ) -> Self {
        Self { op: Operation::Write(off, len), regions, marker: None }
    }

    pub fn new_flush() -> Self {
        let op = Operation::Flush;
        Self { op, regions: Vec::new(), marker: None }
    }

    /// Type of operation being issued.
    pub fn oper(&self) -> Operation {
        self.op
    }

    /// Guest memory regions underlying the request
    pub fn regions(&self) -> &[GuestRegion] {
        &self.regions[..]
    }

    pub fn mappings<'a>(&self, mem: &'a MemCtx) -> Option<Vec<SubMapping<'a>>> {
        match &self.op {
            Operation::Read(..) => {
                self.regions.iter().map(|r| mem.writable_region(r)).collect()
            }
            Operation::Write(..) => {
                self.regions.iter().map(|r| mem.readable_region(r)).collect()
            }
            Operation::Flush => None,
        }
    }

    /// Indicate disposition of completed request
    pub fn complete(mut self, res: Result) {
        if let Some(marker) = self.marker.take() {
            marker.complete(res);
        }
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        if self.marker.is_some() {
            panic!("request dropped prior to completion");
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

    /// Retrieve the next request (if any)
    fn next(&self) -> Option<Request>;

    /// Complete processing of result
    fn complete(&self, res: Result, id: ReqId);

    /// Attach a callback to be run on completion of I/Os.
    ///
    /// Returns whether there was a previously-registered callback.
    fn on_completion(&self, _cb: Box<dyn CompletionCallback>) -> bool {
        false
    }

    /// Get an accessor to guest memory via the underlying device
    fn accessor_mem(&self) -> MemAccessor;

    /// Optional on-attach handler to update device state with new `DeviceInfo`
    fn on_attach(&self, _info: DeviceInfo) {}
}

/// Top-level trait for block backends which will attach to [Device]s in order
/// to process [Request]s posted by the guest.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Access to the [BackendAttachment] representing this backend.
    fn attachment(&self) -> &BackendAttachment;

    /// Query [DeviceInfo] from the backend
    fn info(&self) -> DeviceInfo;

    /// Start attempting to process [Request]s from [Device] (if attached)
    ///
    /// Spawning of any tasks required to do such request processing can be done
    /// as part of this start-up.
    async fn start(&self) -> anyhow::Result<()>;

    /// Stop attempting to process new [Request]s from [Device] (if attached)
    ///
    /// Any in-flight processing of requests should be concluded before this
    /// call returns.
    ///
    /// If any tasks were spawned as part of [Backend::start()], they should be
    /// brought to rest as part of this call.
    async fn stop(&self) -> ();

    /// Attempt to detach from associated [Device]
    ///
    /// Any attached backend should be [stopped](Backend::stop()) and detached
    /// prior to its references being dropped.  An attached [Backend]/[Device]
    /// pair holds mutual references and thus will not be reaped if all other
    /// external references are dropped.
    fn detach(&self) -> std::result::Result<(), DetachError> {
        self.attachment().detach()
    }
}

pub enum CacheMode {
    Synchronous,
    WriteBack,
}

/// Unique ID assigned (by [`tracking::Tracking`] to a given block [`Request`].
#[derive(Copy, Clone, PartialEq, PartialOrd, Eq, Ord)]
pub struct ReqId(u64);
impl ReqId {
    const START: Self = ReqId(1);

    fn advance(&mut self) {
        self.0 += 1;
    }
}
impl Borrow<u64> for ReqId {
    fn borrow(&self) -> &u64 {
        &self.0
    }
}
impl From<ReqId> for u64 {
    fn from(value: ReqId) -> Self {
        value.0
    }
}
