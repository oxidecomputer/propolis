// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A virtual "address space" for model-specific registers (MSRs).
//!
//! MSRs provide system software with a way to configure or interact with the
//! underlying CPU in an extensible way that (as the name suggests) may be
//! specific to a particular range of CPU models. Some MSRs are architectural
//! parts of the x86-64 architecture, though many really are manufacturer- and
//! model-specific.
//!
//! This module provides the [`MsrSpace`] type, which provides a virtual
//! "address space" that other Propolis components can use to register to handle
//! RDMSR/WRMSR operations. Some architectural MSRs are handled entirely in
//! bhyve; those that are handled in Propolis are dispatched to the calling
//! CPU's MSR space for possible handling.
//!
//! Individual handlers are responsible for keeping track of the values that are
//! written to the MSRs they manage (including saving and restoring them during
//! live migration).

use std::sync::{Arc, Mutex};

use crate::util::aspace::{ASpace, Error as ASpaceError};
use thiserror::Error;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn msr_read(
        id: u32,
        val: u64,
        handler_registered: u8,
        handler_ok: u8,
        disposition: u8,
    ) {
    }

    fn msr_write(
        id: u32,
        val: u64,
        handler_registered: u8,
        handler_ok: u8,
        disposition: u8,
    ) {
    }
}

/// A handler for MSR operations.
///
/// # Arguments
///
/// - `MsrId`: The ID of the MSR being read or written.
/// - `MsrOp`: The operation to perform on the supplied MSR.
///
/// # Return value
///
/// - `Ok(disposition)` if the handler successfully processed the operation. The
///   enclosed [`MsrDisposition`] tells the caller if further action is
///   required.
/// - `Err` if the handler function encountered an internal error. The operation
///   is completely unhandled; in particular, if it was a [`MsrOp::Read`], no
///   output value was written.
pub type MsrFn = dyn Fn(MsrId, MsrOp) -> anyhow::Result<MsrDisposition>
    + Send
    + Sync
    + 'static;

/// The 32-bit identifier for a specific MSR.
#[derive(Clone, Copy, Debug)]
pub struct MsrId(pub u32);

/// An operation on an MSR.
pub enum MsrOp<'a> {
    /// The guest executed RDMSR. The returned value (if any) is written to the
    /// supplied `u64`.
    Read(&'a mut u64),

    /// The guest executed WRMSR and passed the supplied `u64` as an operand.
    Write(u64),
}

/// The disposition of an operation on a MSR.
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum MsrDisposition {
    /// The MSR operation was handled and no further action is needed from the
    /// caller.
    Handled = 0,

    /// The caller should inject #GP into the RDMSR/WRMSR-executing vCPU.
    GpException = 1,
}

/// Errors that can arise while trying to dispatch an MSR operation.
#[derive(Debug, Error)]
pub enum Error {
    #[error("no handler registered for MSR {0:#x}")]
    HandlerNotFound(u32),

    #[error("address space operation failed")]
    ASpace(#[from] ASpaceError),

    #[error("error from MSR handler")]
    HandlerError(anyhow::Error),
}

/// Manages the virtual MSR "address space".
pub struct MsrSpace {
    /// The mapping from MSR IDs to handler functions.
    map: Mutex<ASpace<Arc<MsrFn>>>,
}

impl MsrSpace {
    /// Creates a new MSR space.
    pub fn new() -> Self {
        Self { map: Mutex::new(ASpace::new(0, u32::MAX as usize)) }
    }

    /// Registers `func` as the handler for the range of MSRs in
    /// [`start`..`len`).
    pub fn register(
        &self,
        start: MsrId,
        len: u32,
        func: Arc<MsrFn>,
    ) -> Result<(), Error> {
        Ok(self.map.lock().unwrap().register(
            start.0 as usize,
            len as usize,
            func,
        )?)
    }

    /// Unregisters the MSR handler that passed `base` as the starting MSR when
    /// it called [`Self::register`].
    pub fn unregister(&self, base: MsrId) -> Result<(), Error> {
        self.map.lock().unwrap().unregister(base.0 as usize)?;
        Ok(())
    }

    /// Handles the RDMSR instruction.
    pub fn rdmsr(
        &self,
        msr: MsrId,
        out: &mut u64,
    ) -> Result<MsrDisposition, Error> {
        let res = self.do_msr_op(msr, MsrOp::Read(out));
        probes::msr_read!(|| {
            let info = ProbeInfo::from(&res);
            let (ok, disposition) = if let Some(d) = info.disposition {
                (true, d as u8)
            } else {
                (false, 0)
            };
            (msr.0, *out, info.registered as u8, ok as u8, disposition)
        });
        res
    }

    /// Handles the WRMSR instruction.
    pub fn wrmsr(
        &self,
        msr: MsrId,
        value: u64,
    ) -> Result<MsrDisposition, Error> {
        let res = self.do_msr_op(msr, MsrOp::Write(value));
        probes::msr_write!(|| {
            let info = ProbeInfo::from(&res);
            let (ok, disposition) = if let Some(d) = info.disposition {
                (true, d as u8)
            } else {
                (false, 0)
            };
            (msr.0, value, info.registered as u8, ok as u8, disposition)
        });
        res
    }

    /// Handles MSR operations.
    fn do_msr_op(
        &self,
        msr: MsrId,
        op: MsrOp,
    ) -> Result<MsrDisposition, Error> {
        let map = self.map.lock().unwrap();
        let handler = match map.region_at(msr.0 as usize) {
            Ok((_start, _len, f)) => f,
            Err(ASpaceError::NotFound) => {
                return Err(Error::HandlerNotFound(msr.0));
            }
            Err(e) => {
                unreachable!("unexpected error {e} from MSR space lookup");
            }
        };

        let handler = Arc::clone(handler);

        // Allow other vCPUs to access the handler map while this operation is
        // being processed.
        drop(map);
        handler(msr, op).map_err(Error::HandlerError)
    }
}

/// A helper type for converting results from [`MsrSpace::do_msr_op`] into
/// USDT probe inputs.
struct ProbeInfo {
    /// True if there was a handler registered for the target MSR.
    registered: bool,

    /// `Some(disposition)` if the handler succeeded, `None` if it failed.
    disposition: Option<MsrDisposition>,
}

impl From<&Result<MsrDisposition, Error>> for ProbeInfo {
    fn from(value: &Result<MsrDisposition, Error>) -> Self {
        match value {
            Ok(d) => Self { registered: true, disposition: Some(*d) },
            Err(Error::HandlerNotFound(_)) => {
                Self { registered: false, disposition: None }
            }
            Err(Error::HandlerError(_)) => {
                Self { registered: true, disposition: None }
            }
            Err(Error::ASpace(_)) => unreachable!(
                "shouldn't get an ASpaceError while handling MSR ops"
            ),
        }
    }
}
