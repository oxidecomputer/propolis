// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// A model-specific register (MSR) number.
#[derive(Clone, Copy, Debug)]
pub struct MsrId(pub u32);

/// An outcome resulting from a request to emulate the RDMSR instruction.
#[derive(Clone, Copy, Debug)]
pub enum RdmsrOutcome {
    /// This RDMSR was not handled. The caller must decide how to dispose of it.
    NotHandled,

    /// This read was handled and produced the contained value, which should be
    /// returned to the guest.
    Handled(u64),

    /// This read is illegal. The caller should inject #GP into the CPU that
    /// attempted it.
    GpException,
}

/// An outcome resulting from a request to emulate the WRMSR instruction.
#[derive(Clone, Copy, Debug)]
pub enum WrmsrOutcome {
    /// This WRMSR was not handled. The caller must decide how to dispose of it.
    NotHandled,

    /// This write was handled and no further action is needed from the caller.
    Handled,

    /// This write is illegal. The caller should inject #GP into the CPU that
    /// attempted it.
    GpException,
}
