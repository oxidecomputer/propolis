// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for the Hyper-V reference time enlightenment. See TLFS section 12.7.
//!
//! # Theory
//!
//! The x86 timestamp counter (TSC) gives system software a high-resolution
//! performance counter that increments roughly once per processor clock cycle.
//! The TSC is just a counter and does not return elapsed time in SI units;
//! instead, readers convert TSC values to elapsed time by dividing the number
//! of TSC ticks by the TSC frequency to get a number of seconds, which they can
//! then convert to a reference frequency:
//!
//! ```text
//! elapsed reference units
//!     = elapsed seconds * (reference units / 1 sec)
//!     = TSC ticks * (1 / (TSC ticks / 1 sec)) * reference frequency
//!     = TSC ticks * (1 / TSC frequency) * reference frequency
//!     = TSC ticks * (reference frequency / TSC frequency)
//! ```
//!
//! (This calculation assumes that the TSC does in fact tick at a constant
//! frequency. This is often the case on modern processors, but it is not
//! guaranteed, and system software is expected to check CPUID to see if the CPU
//! advertises such an "invariant" TSC before doing this kind of calculation.)
//!
//! KVM and Linux use nanoseconds as the reference time unit and so have a
//! reference frequency of 1e9 ticks/sec. Windows and Hyper-V use 100ns units
//! and a frequency of 1e7 ticks/sec. Because these frequencies are expressed in
//! cycles per second, using simple integer divisions to convert ticks to
//! seconds or to scale frequencies will lose all sub-second precision, which
//! precision is of course the point of having a high-resolution timekeeping
//! facility. To avoid this problem without having to use floating-point
//! arithmetic, timekeeping enlightenments usually turn to fixed-point scaling
//! fractions.
//!
//! The general idea is to take an N-bit frequency value and represent it as a
//! 2N-bit value with an implicit radix point between bits N and N-1, such that
//! the high N bits of the value are its integer part and the low N bits are its
//! fractional part. That is, the idea is to re-express the frequency multiplier
//! term in the conversion above as follows:
//!
//! ```text
//! frequency multiplier
//!     = reference frequency / TSC frequency
//!     = ((reference frequency) * 2^N) / TSC frequency) * (1 / 2^N)
//! ```
//!
//! In the present case, N = 64, and the `reference frequency * 2^N` term is a
//! fixed-point number with 64 integer bits and 64 fractional bits. Notice that
//! now, dividing by the TSC frequency will not truncate to 0; instead, the
//! 64 highest-order bits of the fractional portion of the quotient will be
//! preserved in the low 64 bits of the result of this 128-bit integer division.
//!
//! Since this scaling factor is represented as an integer, a TSC reader can
//! simply multiply and shift to get an elapsed tick count:
//!
//! ```text
//! elapsed reference units
//!     = TSC ticks * (reference frequency / TSC frequency)
//!     = (TSC ticks * scaling factor) / 2^64
//!     = (TSC ticks * scaling factor) >> 64
//! ```
//!
//! There is one small catch: the scaling factor is nominally a 128-bit integer,
//! but the x86-64 `IMUL` instruction's maximum operand size is 64 bits. There
//! are several ways around this; Hyper-V's is to observe that if the host TSC
//! frequency is greater than or equal to 10 MHz, then scaling factor will be
//! less than 1, which means its integer portion is 0, which means that the `TSC
//! ticks * scaling factor` factor can be trivially rewritten as the 128-bit
//! product of a 64-bit TSC value and the low 64 bits (the fractional bits) of
//! the scaling factor.
//!
//! # Practice
//!
//! Hyper-V provides an overlay page that contains a 64-bit scaling factor and
//! an offset that a guest can use to convert a guest TSC reading to the time
//! since guest boot in 100-nanosecond units. Section 12.7.3 of the TLFS
//! specifies the following computation:
//!
//! ```text
//! reference_time: u128 = ((tsc * scale) >> 64) + offset
//! ```
//!
//! The host computes the `scale` factor by shifting the reference frequency
//! (1e7) left by 64 places and dividing by the guest's effective TSC frequency
//! to get a scaling fraction, as described above. The `offset` depends on the
//! difference between the host and guest TSC values; this implementation
//! assumes that bhyve will set up the guest such that this offset can always be
//! 0 (i.e., the guest will obtain an appropriately-offset TSC value directly
//! from RDTSC without having to correct it further).
//!
//! Although unlikely on the machines Propolis generally targets, it is
//! theoretically possible for the host TSC frequency to be so low that the
//! scaling factor cannot be expressed as a 0.64 fixed-point fraction. In this
//! case the hypervisor writes a special value to the TSC page's `sequence`
//! field to denote that the rest of the page's contents are invalid. See
//! [`ReferenceTscPage`] for more details.
//!
//! # Live migration
//!
//! When a VM migrates from one host to another, it will usually find that the
//! hosts' TSC values are not in sync, either because they were started at
//! different times or they have different TSC frequencies (or both).
//!
//! Propolis accounts for these differences using hardware TSC scaling and
//! offset features. These are similar to the scale and offset fields on the
//! reference page: the hypervisor programs a fixed-point scaling multiplier and
//! offset into the VM's control structures before entering the guest, and the
//! processor applies these factors when the guest executes RDTSC.
//!
//! This module assumes that if its VM is migrated, the overarching migration
//! protocol will ensure that the guest's observed TSC frequency and offset will
//! remain unchanged, such that the reference TSC page's contents can remain
//! unchanged when a VM migrates. (The propolis-server migration protocol
//! ensures this by requiring migration targets to support hardware-based TSC
//! scaling and offsetting.)

use crate::{
    common::{GuestAddr, PAGE_MASK, PAGE_SHIFT, PAGE_SIZE},
    vmm::Pfn,
};

use zerocopy::AsBytes;

const ENABLED_BIT: u64 = 0;
const ENABLED_MASK: u64 = 1 << ENABLED_BIT;

/// Represents a value written to the [`HV_X64_MSR_REFERENCE_TSC`] register.
///
/// [`HV_X64_MSR_REFERENCE_TSC`]: super::bits::HV_X64_MSR_REFERENCE_TSC
#[derive(Clone, Copy, Default)]
pub(super) struct MsrReferenceTscValue(pub(super) u64);

impl std::fmt::Debug for MsrReferenceTscValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsrReferenceTscValue")
            .field("raw", &self.0)
            .field("raw", &format!("{:#x}", self.0))
            .field("gpa", &format!("{:#x}", self.gpa().0))
            .field("enabled", &self.enabled())
            .finish()
    }
}

impl MsrReferenceTscValue {
    /// Yields the PFN at which the guest would like to place the reference TSC
    /// page.
    pub fn gpfn(&self) -> Pfn {
        Pfn::new(self.0 >> PAGE_SHIFT).unwrap()
    }

    /// Yields the guest physical address at which the guest would like to place
    /// the reference TSC page.
    pub fn gpa(&self) -> GuestAddr {
        GuestAddr(self.0 & PAGE_MASK as u64)
    }

    /// Returns `true` if the reference TSC overlay is enabled.
    pub fn enabled(&self) -> bool {
        (self.0 & ENABLED_MASK) != 0
    }
}

/// The contents of a reference TSC page, defined in TLFS section 12.7.2.
#[derive(Clone, Copy, Debug, Default, AsBytes)]
#[repr(packed, C)]
pub(super) struct ReferenceTscPage {
    /// Incremented whenever the `scale` or `offset` fields of this page are
    /// modified. Guests are meant to read the sequence value, read the scale
    /// and offset fields, and re-read the sequence value, consuming the scale
    /// and offset only if the sequence did not change.
    ///
    /// If this value is 0, guests are not to use the scale and offset factors
    /// on this page and are to fall back to another time source.
    ///
    /// This module assumes that if a VM migrates, the overarching migration
    /// protocol will work with bhyve to ensure that the guest TSC offset and
    /// observed frequency remain unchanged.
    sequence: u32,

    /// Reserved for alignment.
    reserved: u32,

    /// The 0.64 fixed-point scaling factor to use to convert guest TSC ticks
    /// into 100-nanosecond time units. This is computed as `((10_000_000u128 <<
    /// 64) / guest_tsc_frequency`. If this value cannot be represented as a
    /// 0.64 fixed-point fraction, the reference TSC page is disabled.
    scale: u64,

    /// The offset, in 100 ns units, that the guest should add to its scaled TSC
    /// readings to obtain the number of 100 ns units that have elapsed since
    /// the guest booted.
    ///
    /// This implementation assumes that bhyve ensures that the guest TSC is
    /// always correctly offset from the host TSC, so it always sets this value
    /// to 0.
    offset: i64,
}

impl ReferenceTscPage {
    /// Creates reference TSC data with a scaling factor computed from the
    /// supplied guest TSC frequency.
    pub(super) fn new(guest_freq: u64) -> Self {
        let (scale, sequence) =
            if let Some(scale) = guest_freq_to_scale(guest_freq) {
                (scale, 1)
            } else {
                (0, 0)
            };

        Self { sequence, scale, ..Default::default() }
    }
}

impl From<&ReferenceTscPage> for Box<[u8; PAGE_SIZE]> {
    fn from(value: &ReferenceTscPage) -> Self {
        let mut page = Box::new([0u8; PAGE_SIZE]);
        page[0..std::mem::size_of::<ReferenceTscPage>()]
            .copy_from_slice(value.as_bytes());

        page
    }
}

/// Converts the supplied guest TSC frequency into a 0.64 fixed-point scaling
/// factor. Returns `None` if the correct factor cannot be so expressed.
fn guest_freq_to_scale(guest_freq: u64) -> Option<u64> {
    const HUNDRED_NS_PER_SEC: u128 = 10_000_000;
    let scale: u128 = (HUNDRED_NS_PER_SEC << 64) / guest_freq as u128;
    if (scale >> 64) != 0 {
        None
    } else {
        Some(scale as u64)
    }
}
