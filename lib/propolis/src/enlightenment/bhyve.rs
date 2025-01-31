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

/// An implementation of the bhyve guest-hypervisor interface. This interface
/// exposes no special enlightenments; its only purpose is to inject the
/// appropriate hypervisor ID into CPUID leaf 0x4000_0000, since this leaf will
/// not otherwise appear in a propolis-server instance specification's CPUID
/// settings.
pub struct BhyveGuestInterface;

impl Lifecycle for BhyveGuestInterface {
    fn type_name(&self) -> &'static str {
        "bhyve-guest-interface"
    }
}

impl Enlightenment for BhyveGuestInterface {
    fn add_cpuid(&self, cpuid: &mut CpuidSet) -> anyhow::Result<()> {
        match cpuid.insert(
            CpuidIdent::leaf(HYPERVISOR_BASE_LEAF),
            // Leaf 0x4000_0000 is the maximum hypervisor leaf. "bhyve bhyve "
            // is the vendor ID, split across ebx/ecx/edx.
            CpuidValues {
                eax: HYPERVISOR_BASE_LEAF,
                ebx: 0x76796862,
                ecx: 0x68622065,
                edx: 0x20657679,
            },
        ) {
            Ok(None) => Ok(()),

            // Return an error if the key was previously present in the map,
            // regardless of whether the `CpuidSet` was willing to replace its
            // value.
            Ok(Some(_)) | Err(_) => {
                Err(anyhow::anyhow!("reserved leaf 0x4000_0000 already in map"))
            }
        }
    }

    fn rdmsr(&self, _vcpu: VcpuId, _msr: MsrId) -> RdmsrOutcome {
        RdmsrOutcome::NotHandled
    }

    fn wrmsr(&self, _vcpu: VcpuId, _msr: MsrId, _value: u64) -> WrmsrOutcome {
        WrmsrOutcome::NotHandled
    }

    fn attach(&self, _parent: &MemAccessor) {}
}
