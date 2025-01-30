// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Provides a bhyve-compatible guest-hypervisor interface.
//!
//! This interface supplies no special enlightenments; it merely identifies
//! itself as a bhyve hypervisor in CPUID leaf 0x4000_0000.

use cpuid_utils::{
    bits::HYPERVISOR_BASE_LEAF, CpuidIdent, CpuidSet, CpuidValues,
};

use crate::{
    accessors::MemAccessor,
    common::{Lifecycle, VcpuId},
    enlightenment::Enlightenment,
    msr::{MsrId, RdmsrOutcome, WrmsrOutcome},
};

pub struct BhyveGuestInterface;

impl Lifecycle for BhyveGuestInterface {
    fn type_name(&self) -> &'static str {
        "bhyve-guest-interface"
    }
}

impl Enlightenment for BhyveGuestInterface {
    fn add_cpuid(&self, cpuid: &mut CpuidSet) -> anyhow::Result<()> {
        let old = cpuid
            .insert(
                CpuidIdent::leaf(HYPERVISOR_BASE_LEAF),
                CpuidValues {
                    eax: HYPERVISOR_BASE_LEAF,
                    ebx: 0x76796862,
                    ecx: 0x68622065,
                    edx: 0x20657679,
                },
            )
            .map_err(|_| {
                anyhow::anyhow!("reserved leaf 0x4000_0000 already in map")
            })?;

        if old.is_some() {
            anyhow::bail!("reserved leaf 0x4000_0000 already in map");
        }

        Ok(())
    }

    fn rdmsr(&self, _msr: MsrId, _vcpu: VcpuId) -> RdmsrOutcome {
        RdmsrOutcome::NotHandled
    }

    fn wrmsr(&self, _msr: MsrId, _vcpu: VcpuId, _value: u64) -> WrmsrOutcome {
        WrmsrOutcome::NotHandled
    }

    fn set_parent_mem_accessor(&self, _parent: &MemAccessor) {}
}
