// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Constant definitions and flags for bhyve enlightenments.

use cpuid_utils::{CpuidIdent, CpuidValues};

/// bhyve hypervisor identifier at CPUID leaf 0x4000_0000. bhyve only supports
/// 1 leaf in the hypervisor range ([`BHYVE_FEATURES_CPUID_LEAF_1`]) which is
/// returned in eax with ebx/ecx/edx containing the vendor ID: "bhyve bhyve ".
pub(super) const BHYVE_LEAF_0_VALUES: CpuidValues = CpuidValues {
    eax: BHYVE_FEATURES_CPUID_LEAF_1.leaf,
    ebx: 0x76796862,
    ecx: 0x68622065,
    edx: 0x20657679,
};

/// bhyve leaf 0x4000_0001 describes features supported by the hypervisor that
/// a guest may take advantage of in eax.
pub(super) const BHYVE_FEATURES_CPUID_LEAF_1: CpuidIdent =
    CpuidIdent { leaf: 0x4000_0001, subleaf: None };

bitflags::bitflags! {
    /// bhyve leaf 0x4000_0001 eax details the supported set of features.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(super) struct BhyveLeaf1Eax: u32 {
        const EXT_MSI_DEST_ID = 1 << 0;

        // Bits 1-31 are reserved.
    }
}
