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
//! KVM and Linux use nanoseconds as the reference time unit, so their reference
//! frequency is 1e9 ticks/sec. Windows and Hyper-V use 100ns units and a
//! frequency of 1e7 ticks/sec. Because these frequencies are expressed in
//! cycles per second, using simple integer divisions to convert ticks to
//! seconds or to scale frequencies will lose all sub-second precision, which
//! defeats the purpose of having a high-resolution timekeeping facility. To
//! avoid this problem without having to use floating-point arithmetic,
//! timekeeping enlightenments usually turn to fixed-point scaling fractions.
//!
//! The idea (at least in this enlightenment) is to take a 64-bit frequency
//! value and represent it as a 128-bit integer with an implicit radix point
//! between bits 63 and 64. The upper 64 bits of the value are its integer part,
//! and the lower 64 bits are its fractional part. Doing this to the integer
//! reference frequency in the conversion above amounts to writing the
//! following:
//!
//! ```text
//! frequency multiplier
//!     = reference frequency / TSC frequency
//!     = ((reference frequency as u128) * 2^64) / TSC frequency) * (1 / 2^64)
//! ```
//!
//! The first term of this multiplication is a 128-bit scaling factor. Notice
//! that, because the TSC frequency is a 64-bit integer and therefore guaranteed
//! to be less than 2^64, the division in the first term won't truncate to 0;
//! instead, if the quotient has a fractional part, the high 64 bits of that
//! fractional part will be preserved in the low 64 bits of the integer
//! division quotient.
//!
//! The scaling factor is still an integer, so a TSC reader that wants to apply
//! it can do so with an integer multiplication followed by a shift:
//!
//! ```text
//! elapsed reference units
//!     = TSC ticks * (reference frequency / TSC frequency)
//!     = (TSC ticks * scaling factor) / 2^64
//!     = (TSC ticks * scaling factor) >> 64
//! ```
//!
//! There is one small catch: the scaling factor was computed as a 128-bit
//! value, but the x86-64 `IMUL` instruction's maximum operand size is 64 bits.
//! This enlightenment avoids this problem by observing that if the host TSC
//! frequency is greater than 10 MHz (highly likely on a platform with an
//! invariant TSC), then the scaling factor is less than 1, which means its
//! upper 64 bits are 0, which means that the scaled TSC value can be trivially
//! rewritten as the product of a 64-bit TSC value and the lower 64 bits of the
//! scaling factor. This 128-bit product can then be shifted right by 64 bits to
//! produce an elapsed time.
//!
//! If the host TSC frequency is too low for the scaling factor to fit in 64
//! bits, Hyper-V simply disables the enlightenment by writing a special value
//! to the reference page. Other hypervisors like KVM may handle the situation
//! differently, e.g. by having the guest shift its TSC readings before
//! multiplying by the scaling factor to guarantee that the product won't
//! overflow.
//!
//! Although this discussion focused on 64.64 fixed-point fractions, the same
//! principles can be applied for values of different widths and different radix
//! points. For example, Intel processors that support TSC scaling use a 64-bit
//! scaling value with 16 integer bits and 48 fractional bits.
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

use std::sync::Arc;

use crate::{
    common::{GuestAddr, PAGE_MASK, PAGE_SHIFT, PAGE_SIZE},
    enlightenment::hyperv::{
        overlay::{OverlayKind, OverlayManager},
        TscOverlay,
    },
    vmm::Pfn,
};

use zerocopy::{Immutable, IntoBytes};

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
#[derive(Clone, Copy, Debug, Default, IntoBytes, Immutable)]
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

/// The enablement status of a reference TSC enlightenment.
#[derive(Clone, Copy, Debug)]
pub(super) enum ReferenceTsc {
    /// The enlightenment is disabled.
    Disabled,

    /// The enlightenment is enabled, but has not yet been initialized.
    Uninitialized,

    /// The enlightenment is enabled and initialized.
    Enabled { msr_value: MsrReferenceTscValue, guest_freq: u64 },
}

impl ReferenceTsc {
    /// Returns `true` if the reference TSC enlightenment is present in this
    /// Hyper-V stack, regardless of whether it has been initialized yet.
    pub(super) fn is_present(&self) -> bool {
        matches!(
            self,
            ReferenceTsc::Uninitialized | ReferenceTsc::Enabled { .. }
        )
    }

    /// Sets this enlightenment's reference TSC MSR value.
    ///
    /// # Panics
    ///
    /// Panics if this enlightenment is not enabled and fully initialized.
    pub(super) fn set_msr_value(&mut self, value: MsrReferenceTscValue) {
        match self {
            Self::Enabled { msr_value, .. } => *msr_value = value,
            _ => panic!(
                "setting TSC MSR value for invalid enlightenment {self:?}"
            ),
        }
    }

    /// Registers a reference TSC overlay page with the supplied overlay manager
    /// at the PFN specified by this struct's `msr_value`.
    ///
    /// # Return value
    ///
    /// `Some` if an overlay was successfully created at the relevant PFN.
    /// `None` if the MSR value indicates the overlay is disabled or if the
    /// overlay could not be created at the requested PFN.
    ///
    /// # Panics
    ///
    /// Panics if this enlightenment is not enabled and fully initialized.
    pub(super) fn create_overlay(
        &self,
        overlay_manager: &Arc<OverlayManager>,
    ) -> Option<TscOverlay> {
        let Self::Enabled { msr_value, guest_freq } = self else {
            panic!(
                "asked to create a TSC overlay for invalid enlightenment \
                {self:?}"
            );
        };

        if !msr_value.enabled() {
            return None;
        }

        let page = ReferenceTscPage::new(*guest_freq);
        overlay_manager
            .add_overlay(msr_value.gpfn(), OverlayKind::ReferenceTsc(page))
            .ok()
            .map(TscOverlay)
    }
}
