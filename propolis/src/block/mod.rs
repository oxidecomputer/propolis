//! Implements an interface to virtualized block devices.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::common::*;
use crate::dispatch::{AsyncCtx, DispCtx, Dispatcher};
use crate::vmm::{MemCtx, SubMapping};

use tokio::sync::Notify;

mod file;
pub use file::FileBackend;

pub type ByteOffset = usize;
pub type ByteLen = usize;

/// Type of operations which may be issued to a virtual block device.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Operation {
    /// Read from offset
    Read(ByteOffset),
    /// Write to offset
    Write(ByteOffset),
    /// Flush buffer(s) for [offset, offset + len)
    Flush(ByteOffset, ByteLen),
}

#[derive(Copy, Clone, Debug)]
pub enum Result {
    Success,
    Failure,
    Unsupported,
}

pub type CompleteFn = dyn FnOnce(Result, &DispCtx) + Send + Sync + 'static;

/// Trait indicating that a type may be used as a request to a block device.
pub struct Request {
    op: Operation,
    regions: Vec<GuestRegion>,
    donef: Option<Box<CompleteFn>>,
}
impl Request {
    pub fn new_read(
        off: usize,
        regions: Vec<GuestRegion>,
        donef: Box<CompleteFn>,
    ) -> Self {
        let op = Operation::Read(off);
        Self { op, regions, donef: Some(donef) }
    }

    pub fn new_write(
        off: usize,
        regions: Vec<GuestRegion>,
        donef: Box<CompleteFn>,
    ) -> Self {
        let op = Operation::Write(off);
        Self { op, regions, donef: Some(donef) }
    }

    pub fn new_flush(off: usize, len: usize, donef: Box<CompleteFn>) -> Self {
        let op = Operation::Flush(off, len);
        Self { op, regions: Vec::new(), donef: Some(donef) }
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
            Operation::Read(_) => {
                self.regions.iter().map(|r| mem.writable_region(r)).collect()
            }
            Operation::Write(_) => {
                self.regions.iter().map(|r| mem.readable_region(r)).collect()
            }
            Operation::Flush(_, _) => None,
        }
    }

    /// Total length of operation
    pub fn len(&self) -> usize {
        match &self.op {
            Operation::Read(_) | Operation::Write(_) => {
                self.regions.iter().map(|r| r.1).sum()
            }
            Operation::Flush(_, len) => *len,
        }
    }

    /// Indiciate disposition of completed request
    pub fn complete(mut self, res: Result, ctx: &DispCtx) {
        let func = self.donef.take().unwrap();
        func(res, ctx);
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        if self.donef.is_some() {
            panic!("request dropped prior to completion");
        }
    }
}

/// Metadata regarding a virtualized block device.
#[derive(Debug, Copy, Clone)]
pub struct DeviceInfo {
    /// Size (in bytes) per block
    pub block_size: u32,
    /// Device size in blocks (see above)
    pub total_size: u64,
    /// Is the device writable
    pub writable: bool,
}

/// API to access a virtualized block device.
pub trait Device: Send + Sync + 'static {
    /// Retreive the next request (if any)
    fn next(&self, ctx: &DispCtx) -> Option<Request>;

    fn set_notifier(&self, f: Option<Box<NotifierFn>>);
}

pub trait Backend: Send + Sync + 'static {
    fn attach(&self, dev: Arc<dyn Device>, disp: &Dispatcher);
    fn info(&self) -> DeviceInfo;
}

pub type NotifierFn = dyn Fn(&dyn Device, &DispCtx) + Send + Sync + 'static;

pub struct Notifier {
    armed: AtomicBool,
    notifier: Mutex<Option<Box<NotifierFn>>>,
}
impl Notifier {
    pub fn new() -> Self {
        Self { armed: AtomicBool::new(false), notifier: Mutex::new(None) }
    }
    pub fn next_arming(
        &self,
        nextf: impl Fn() -> Option<Request>,
    ) -> Option<Request> {
        self.armed.store(false, Ordering::Release);
        let res = nextf();
        if res.is_some() {
            // Since a result was successfully retrieved, no need to rearm the
            // notification trigger.
            return res;
        }

        // On the off chance that the underlying resource became available after
        // rearming the notification trigger, check again.
        self.armed.store(true, Ordering::Release);
        if let Some(r) = nextf() {
            self.armed.store(false, Ordering::Release);
            Some(r)
        } else {
            None
        }
    }
    pub fn notify(&self, dev: &dyn Device, ctx: &DispCtx) {
        if self.armed.load(Ordering::Acquire) {
            let inner = self.notifier.lock().unwrap();
            if let Some(func) = inner.as_ref() {
                func(dev, ctx);
            }
        }
    }
    pub fn set(&self, val: Option<Box<NotifierFn>>) {
        let mut inner = self.notifier.lock().unwrap();
        *inner = val;
    }
}

pub struct AsyncWaiter {
    wake: Arc<Notify>,
}
impl AsyncWaiter {
    pub fn new(attach_dev: &dyn Device) -> Self {
        let this = Self { wake: Arc::new(Notify::new()) };
        let wake = Arc::clone(&this.wake);
        attach_dev
            .set_notifier(Some(Box::new(move |_dev, _ctx| wake.notify_one())));
        this
    }
    pub async fn next(
        &self,
        dev: &dyn Device,
        actx: &AsyncCtx,
    ) -> Option<Request> {
        loop {
            {
                let ctx = actx.dispctx().await?;
                if let Some(r) = dev.next(&ctx) {
                    return Some(r);
                }
            }
            self.wake.notified().await;
        }
    }
}
