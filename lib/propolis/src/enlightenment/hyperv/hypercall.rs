// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for hypercalls and their related MSRs.

use crate::common::{GuestAddr, PAGE_MASK, PAGE_SHIFT, PAGE_SIZE};

const LOCKED_BIT: u64 = 1;
const LOCKED_MASK: u64 = 1 << LOCKED_BIT;
const ENABLED_BIT: u64 = 0;
const ENABLED_MASK: u64 = 1 << ENABLED_BIT;

/// Represents a value written to the [`HV_X64_MSR_HYPERCALL`] register.
///
/// Writing to this register enables the hypercall page. The hypervisor
/// overwrites this page with an instruction sequence that the guest should
/// execute in order to issue a call to the hypervisor. See
/// [`HYPERCALL_INSTRUCTION_SEQUENCE`].
///
/// Bits 11:2 of this register are reserved. The TLFS specifies that the guest
/// "should ignore [them] on reads and preserve [them] on writes," but imposes
/// no particular penalties on guests that modify these bits.
///
/// [`HV_X64_MSR_HYPERCALL`]: super::bits::HV_X64_MSR_HYPERCALL
#[derive(Clone, Copy, Default)]
pub(super) struct MsrHypercallValue(pub(super) u64);

impl std::fmt::Debug for MsrHypercallValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsrHypercallValue")
            .field("raw", &format!("{:#x}", self.0))
            .field("gpa", &format!("{:#x}", self.gpa().0))
            .field("locked", &self.locked())
            .field("enabled", &self.enabled())
            .finish()
    }
}

impl MsrHypercallValue {
    /// Yields the guest page number (the PFN) at which the guest would like the
    /// hypercall page to be placed.
    pub fn gpfn(&self) -> u64 {
        self.0 >> PAGE_SHIFT
    }

    /// Returns the guest physical address at which the guest would like the
    /// hypercall page to be placed.
    pub fn gpa(&self) -> GuestAddr {
        GuestAddr(self.0 & PAGE_MASK as u64)
    }

    /// Returns whether the hypercall page location is locked. Once locked, the
    /// value in `MSR_HYPERCALL` cannot change until the hypervisor resets the
    /// guest.
    pub fn locked(&self) -> bool {
        (self.0 & LOCKED_MASK) != 0
    }

    /// Indicates whether the hypercall page is enabled.
    pub fn enabled(&self) -> bool {
        (self.0 & ENABLED_MASK) != 0
    }

    /// Clears this value's enabled bit.
    pub fn clear_enabled(&mut self) {
        self.0 &= !ENABLED_MASK;
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
