// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Provides a bhyve-compatible guest-hypervisor interface.
//!
//! This interface indentifies itself as a bhyve hypervisor in CPUID leaf
//! 0x4000_0000 and optional enlightenments.

use std::sync::Arc;

use cpuid_utils::{
    bits::HYPERVISOR_BASE_LEAF, CpuidIdent, CpuidSet, CpuidValues,
};

use crate::{
    accessors::MemAccessor,
    common::{Lifecycle, VcpuId},
    enlightenment::{
        bhyve::bits::{
            BhyveLeaf1Eax, BHYVE_FEATURES_CPUID_LEAF_1, BHYVE_LEAF_0_VALUES,
        },
        AddCpuidError, Enlightenment,
    },
    msr::{MsrId, RdmsrOutcome, WrmsrOutcome},
    vmm::VmmHdl,
};

mod bits;

/// A set of features that can be enabled for a given bhyve enlightenment stack.
#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    /// Enable support for 15-bit Extended MSI Destination IDs
    pub ext_msi_dest_id: bool,
}

/// An implementation of the bhyve guest-hypervisor interface. In addition to
/// any optional enlightenments that are enabled, this interface also serves to
/// inject the appropriate hypervisor ID into CPUID leaf 0x4000_0000, since this
/// leaf will not otherwise appear in a propolis-server instance specification's
/// CPUID settings.
pub struct BhyveGuestInterface {
    features: Features,
}

impl BhyveGuestInterface {
    /// Returns a basic enlightenment stack with just the hypervisor ID.
    pub fn new() -> Self {
        Self { features: Default::default() }
    }

    /// Returns an enlightenment stack with the given set of features. Certain
    /// features may require support from the kernel vmm component and so this
    /// may fail if running on a version without the required capabilities.
    pub fn with_features(features: Features) -> std::io::Result<Self> {
        if features.ext_msi_dest_id {
            if bhyve_api::api_version()? < bhyve_api::ApiVersion::V21 {
                return Err(std::io::ErrorKind::Unsupported.into());
            }
        }
        Ok(Self { features })
    }
}

impl Lifecycle for BhyveGuestInterface {
    fn type_name(&self) -> &'static str {
        "bhyve-guest-interface"
    }
}

impl Enlightenment for BhyveGuestInterface {
    fn add_cpuid(&self, cpuid: &mut CpuidSet) -> Result<(), AddCpuidError> {
        let mut to_add = CpuidSet::new(cpuid.vendor());
        to_add
            .insert(CpuidIdent::leaf(HYPERVISOR_BASE_LEAF), BHYVE_LEAF_0_VALUES)
            .expect("the map was previously empty");

        let mut leaf_1_eax = BhyveLeaf1Eax::default();
        if self.features.ext_msi_dest_id {
            leaf_1_eax |= BhyveLeaf1Eax::EXT_MSI_DEST_ID;
        }
        to_add
            .insert(
                BHYVE_FEATURES_CPUID_LEAF_1,
                CpuidValues { eax: leaf_1_eax.bits(), ..Default::default() },
            )
            .expect("bhyve CPUID values don't conflict");

        super::add_cpuid(cpuid, to_add)
    }

    fn rdmsr(&self, _vcpu: VcpuId, _msr: MsrId) -> RdmsrOutcome {
        RdmsrOutcome::NotHandled
    }

    fn wrmsr(&self, _vcpu: VcpuId, _msr: MsrId, _value: u64) -> WrmsrOutcome {
        WrmsrOutcome::NotHandled
    }

    fn attach(&self, _parent: &MemAccessor, _vmm_hdl: Arc<VmmHdl>) {}
}
