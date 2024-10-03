// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utility functions and types for working with CPUID values.

use std::collections::BTreeMap;

use propolis_types::{CpuidLeaf, CpuidValues};

/// A map from CPUID leaves to CPUID values.
#[derive(Clone, Debug, Default)]
pub struct CpuidMap(pub BTreeMap<CpuidLeaf, CpuidValues>);

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn host_query(leaf: CpuidLeaf) -> CpuidValues {
    let mut res = CpuidValues::default();

    unsafe {
        std::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) res.ebx,
            // select cpuid 0, also specify eax as clobbered
            inout("eax") leaf.leaf => res.eax,
            inout("ecx") leaf.subleaf.unwrap_or(0) => res.ecx,
            out("edx") res.edx,
        );
    }

    res
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub fn host_query(leaf: CpuidLeaf) -> CpuidValues {
    panic!("host CPUID queries only work on x86")
}
