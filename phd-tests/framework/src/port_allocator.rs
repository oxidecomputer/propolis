//! A small allocator for selecting port numbers.

use std::{
    ops::Range,
    sync::atomic::{AtomicU16, Ordering},
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PortAllocatorError {
    #[error("No more ports available")]
    NoMorePorts,
}

pub struct PortAllocator {
    range: Range<u16>,

    /// PHD tests run in a `catch_unwind` block and so require mutable state
    /// that is created outside a test case to be unwind-safe. Allow the port
    /// allocator to be used in this context by guaranteeing that the next
    /// available port can be accessed atomically.
    next: AtomicU16,
}

impl PortAllocator {
    pub fn new(range: Range<u16>) -> Self {
        let start = range.start;
        Self { range, next: AtomicU16::new(start) }
    }

    pub fn next(&self) -> Result<u16, PortAllocatorError> {
        if self.next.load(Ordering::Relaxed) >= self.range.end {
            return Err(PortAllocatorError::NoMorePorts);
        }

        let port = self.next.fetch_add(1, Ordering::Relaxed);
        if port >= self.range.end {
            Err(PortAllocatorError::NoMorePorts)
        } else {
            Ok(port)
        }
    }

    pub fn reset(&self) {
        self.next.store(self.range.start, Ordering::Relaxed);
    }
}
