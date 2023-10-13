// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

mod file_out;
pub mod pollers;
mod sock;

pub use file_out::BlockingFileOutput;
pub use sock::UDSock;

pub type SinkNotifier = Box<dyn Fn(&dyn Sink) + Send + Sync + 'static>;
pub type SourceNotifier = Box<dyn Fn(&dyn Source) + Send + Sync + 'static>;
pub type BlockingSourceConsumer = Box<dyn Fn(&[u8]) + Send + Sync + 'static>;

pub trait Sink: Send + Sync + 'static {
    // XXX: make this slice based
    fn write(&self, data: u8) -> bool;

    /// Set notifier callback for when sink becomes writable.  If that callback acquires any
    /// exclusion resources (locks, etc), they must not be held setting the notifier.
    fn set_notifier(&self, f: Option<SinkNotifier>);
}

pub trait Source: Send + Sync + 'static {
    // XXX: make this slice based
    fn read(&self) -> Option<u8>;

    fn discard(&self, count: usize) -> usize;
    fn set_autodiscard(&self, active: bool);
    /// Set notifier callback for when source becomes readable.  If that callback acquires any
    /// exclusion resources (locks, etc), they must not be held setting the notifier.
    fn set_notifier(&self, f: Option<SourceNotifier>);
}

/// Device which is a source of bytes which must be processed synchronously,
/// lest they be lost in subsequent operations
pub trait BlockingSource: Send + Sync + 'static {
    fn set_consumer(&self, f: Option<BlockingSourceConsumer>);
}

type NotifierFn<T> = dyn Fn(&T) + Send + Sync + 'static;
pub struct NotifierCell<T: ?Sized> {
    is_set: AtomicBool,
    notifier: Mutex<Option<Box<NotifierFn<T>>>>,
}
impl<T: ?Sized> NotifierCell<T> {
    pub fn new() -> Self {
        Self { is_set: AtomicBool::new(false), notifier: Mutex::new(None) }
    }
}
impl NotifierCell<dyn Sink> {
    pub fn set(&self, f: Option<SinkNotifier>) {
        let mut guard = self.notifier.lock().unwrap();
        self.is_set.store(f.is_some(), Ordering::Release);
        *guard = f;
    }
    pub fn notify(&self, sink: &dyn Sink) {
        if self.is_set.load(Ordering::Acquire) {
            let guard = self.notifier.lock().unwrap();
            if let Some(f) = guard.as_ref() {
                f(sink);
            }
        }
    }
}
impl NotifierCell<dyn Source> {
    pub fn set(&self, f: Option<SourceNotifier>) {
        let mut guard = self.notifier.lock().unwrap();
        self.is_set.store(f.is_some(), Ordering::Release);
        *guard = f;
    }
    pub fn notify(&self, source: &dyn Source) {
        if self.is_set.load(Ordering::Acquire) {
            let guard = self.notifier.lock().unwrap();
            if let Some(f) = guard.as_ref() {
                f(source);
            }
        }
    }
}

pub struct ConsumerCell {
    is_set: AtomicBool,
    consumer: Mutex<Option<BlockingSourceConsumer>>,
}
impl ConsumerCell {
    pub fn new() -> Self {
        Self { is_set: AtomicBool::new(false), consumer: Mutex::new(None) }
    }
    pub fn set(&self, f: Option<BlockingSourceConsumer>) {
        let mut guard = self.consumer.lock().unwrap();
        self.is_set.store(f.is_some(), Ordering::Release);
        *guard = f;
    }
    pub fn consume(&self, data: &[u8]) {
        if self.is_set.load(Ordering::Acquire) {
            let guard = self.consumer.lock().unwrap();
            if let Some(f) = guard.as_ref() {
                f(data);
            }
        }
    }
}

/// Build a [`BlockingSourceConsumer`] which silently consumes any output bytes
pub fn null_blocking_consumer() -> BlockingSourceConsumer {
    Box::new(|_buf| {
        // Simply do nothing with the buffer
    })
}
