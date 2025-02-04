// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for hypercalls and their related MSRs.

use crate::common::{GuestAddr, PAGE_SHIFT, PAGE_SIZE};

/// Represents a value written to the [`HV_X64_MSR_HYPERCALL`] register.
///
/// Writing to this register enables the hypercall page. The hypervisor overlays
/// this page with an instruction sequence that the guest should execute in
/// order to issue a call to the hypervisor. See
/// [`HYPERCALL_INSTRUCTION_SEQUENCE`].
///
/// [`HV_X64_MSR_HYPERCALL`]: super::bits::HV_X64_MSR_HYPERCALL
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MsrHypercallValue(pub(super) u64);

impl MsrHypercallValue {
    /// Returns the guest physical page number at which the guest would like the
    /// hypercall page to be placed.
    pub fn gpfn(&self) -> u64 {
        self.0 >> 12
    }

    /// Returns the guest physical address at which the guest would like the
    /// hypercall page to be placed.
    pub fn gpa(&self) -> GuestAddr {
        GuestAddr(self.gpfn() << PAGE_SHIFT)
    }

    /// Returns whether the hypercall page location is locked. Once locked, the
    /// value in `MSR_HYPERCALL` cannot change until the system is reset.
    pub fn locked(&self) -> bool {
        (self.0 & 2) != 0
    }

    /// Indicates whether the hypercall page is enabled.
    pub fn enabled(&self) -> bool {
        (self.0 & 1) != 0
    }

    /// Clears this value's enabled bit.
    pub fn clear_enabled(&mut self) {
        self.0 &= !1;
    }
}

/// The sequence of instructions to write to the hypercall page. This sequence
/// is `mov rax, 2; ret`, which returns a "not supported" status for all
/// hypercalls without actually requiring the guest to exit.
//
// If and when actual hypercall support is required, this should change to
// either `0f 01 c1` (VMCALL) or `0f 01 d9` (VMMCALL), depending on whether the
// host is VMX- or SVM-based.
const HYPERCALL_INSTRUCTION_SEQUENCE: [u8; 8] =
    [0x48, 0xc7, 0xc0, 0x02, 0x00, 0x00, 0x00, 0xc3];

/// Yields a page-sized buffer containing the contents of the hypercall page.
pub(super) fn hypercall_page_contents() -> [u8; PAGE_SIZE] {
    let mut page = [0u8; PAGE_SIZE];
    page[0..8].copy_from_slice(&HYPERCALL_INSTRUCTION_SEQUENCE);
    page
}
