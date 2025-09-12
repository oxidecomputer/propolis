// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Constant definitions and flags for Hyper-V emulations. These are drawn from
//! the Hyper-V TLFS version 6.0b (referred to as "TLFS" below). See the parent
//! module documentation for more details.
//!
//! Where possible, constants in this module (such as MSR identifiers) are given
//! names that match those used in the TLFS.

use cpuid_utils::CpuidValues;

/// Hyper-V-compatible hypervisors are required to support hypervisor CPUID
/// leaves up to 0x4000_0005.
pub(super) const HYPERV_MIN_REQUIRED_CPUID_LEAF: u32 = 0x40000005;

/// CPUID leaf 0x4000_0000 contains hypervisor identifying information. eax
/// receives the highest valid CPUID leaf in the hypervisor range. ebx, ecx, and
/// edx receive a 12-byte vendor ID.
///
/// In order to get both Linux and Windows guests to accept these
/// enlightenments, the ebx/ecx/edx ID here is set to "Microsoft Hv". Windows
/// guests will accept other vendor IDs (they look at leaf 0x4000_0001 eax to
/// identify the hypervisor interface instead of reading the vendor ID in leaf
/// 0), but Linux guests only consider the vendor ID.
const HYPERV_LEAF_0_VALUES: CpuidValues = CpuidValues {
    eax: HYPERV_MIN_REQUIRED_CPUID_LEAF,
    ebx: 0x7263694D,
    ecx: 0x666F736F,
    edx: 0x76482074,
};

/// Generates values for CPUID leaf 0x4000_0000, which contains hypervisor
/// identifying information. eax receives the value of `max_leaf`, the maximum
/// valid CPUID leaf in the hypervisor range; ebx, ecx, and edx contain an
/// appropriate vendor ID.
///
/// `max_leaf` supplies the maximum valid CPUID leaf in the hypervisor range.
///
/// # Panics
///
/// Panics if `max_leaf` is less than [`HYPERV_MIN_REQUIRED_CPUID_LEAF`].
pub(super) fn hyperv_leaf_0_values(max_leaf: u32) -> CpuidValues {
    assert!(
        max_leaf >= HYPERV_MIN_REQUIRED_CPUID_LEAF,
        "requested max leaf {max_leaf:#x} less than minimum required"
    );

    CpuidValues { eax: max_leaf, ..HYPERV_LEAF_0_VALUES }
}

/// Hyper-V leaf 0x4000_0001 contains an (ostensibly vendor-neutral) interface
/// identifier. eax receives "Hv#1"; the other three outputs are reserved.
pub(super) const HYPERV_LEAF_1_VALUES: CpuidValues =
    CpuidValues { eax: 0x31237648, ebx: 0, ecx: 0, edx: 0 };

/// Hyper-V leaf 0x4000_0002 contains hypervisor version information. To avoid
/// having to reason about what it means to expose a specific hypervisor version
/// across a live migration between potentially different host and/or Propolis
/// versions, this information is always set to 0.
pub(super) const HYPERV_LEAF_2_VALUES: CpuidValues =
    CpuidValues { eax: 0, ebx: 0, ecx: 0, edx: 0 };

bitflags::bitflags! {
    /// Hyper-V leaf 0x4000_0003 eax returns synthetic MSR access rights.
    /// Only the bits actually used by this enlightenment stack are enumerated
    /// here.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HyperVLeaf3Eax: u32 {
        const PARTITION_REFERENCE_COUNTER = 1 << 1;
        const HYPERCALL = 1 << 5;
        const VP_INDEX = 1 << 6;
        const PARTITION_REFERENCE_TSC = 1 << 9;

        // Bits 14-31 of this register are reserved.
    }
}

impl Default for HyperVLeaf3Eax {
    /// Grants access to the VP index and hypercall MSRs. This is the minimum
    /// set of access rights that all Hyper-V-compatible hypervisors must grant.
    fn default() -> Self {
        HyperVLeaf3Eax::VP_INDEX | HyperVLeaf3Eax::HYPERCALL
    }
}

/// Hyper-V leaf 0x4000_0004 describes behavior that the guest OS should
/// implement for optimal performance. Propolis expresses no opinion about these
/// options, except that it indicates in ebx that the guest should never try to
/// notify the hypervisor about failed spinlock acquisitions.
pub(super) const HYPERV_LEAF_4_VALUES: CpuidValues =
    CpuidValues { eax: 0, ebx: 0xFFFFFFFF, ecx: 0, edx: 0 };

/// Hyper-V leaf 0x4000_0005 describes the hypervisor's CPU and interrupt
/// remapping limits. Hypervisors are allowed not to expose these limits by
/// publishing 0s to this leaf.
pub(super) const HYPERV_LEAF_5_VALUES: CpuidValues =
    CpuidValues { eax: 0, ebx: 0, ecx: 0, edx: 0 };

/// Allows the guest to report its type and version information. See TLFS
/// section 2.6 for details about this MSR's format.
///
/// Guest OSes are required to identify themselves via this MSR before they can
/// set the enabled bit in [`HV_X64_MSR_HYPERCALL`] or make any hypercalls.
///
/// Read-write; requires the [`HyperVLeaf3Eax::HYPERCALL`] privilege.
pub(super) const HV_X64_MSR_GUEST_OS_ID: u32 = 0x4000_0000;

/// Specifies the guest physical address at which the guest would like to place
/// the hypercall page. See TLFS section 3.13 and the [`MsrHypercallValue`]
/// struct.
///
/// Read-write; requires the [`HyperVLeaf3Eax::HYPERCALL`] privilege.
///
/// [`MsrHypercallValue`]: super::hypercall::MsrHypercallValue
pub(super) const HV_X64_MSR_HYPERCALL: u32 = 0x4000_0001;

/// Guests may read this register to obtain the index of the vCPU that read the
/// register.
///
/// Read-only; requires the [`HyperVLeaf3Eax::VP_INDEX`] privilege.
pub(super) const HV_X64_MSR_VP_INDEX: u32 = 0x4000_0002;

/// Guests may read this register to obtain the time since this VM was created,
/// in 100-nanosecond units.
///
/// Read-only; requires the [`HyperVLeaf3Eax::PARTITION_REFERENCE_COUNTER`]
/// privilege.
pub(super) const HV_X64_MSR_TIME_REF_COUNT: u32 = 0x4000_0020;

/// Specifies the guest physical address at which the guest would like to place
/// the reference TSC page. See TLFS section 12.7 and the
/// [`MsrReferenceTscValue`] struct.
///
/// Read-write; requires the [`HyperVLeaf3Eax::PARTITION_REFERENCE_TSC`]
/// privilege.
///
/// [`MsrReferenceTscValue`]: super::tsc::MsrReferenceTscValue
pub(super) const HV_X64_MSR_REFERENCE_TSC: u32 = 0x4000_0021;
