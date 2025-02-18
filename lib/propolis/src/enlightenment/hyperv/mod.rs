// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for Microsoft Hyper-V emulation.
//!
//! Windows guests and many Linux guests can interoperate with hypervisors that
//! implement the hypervisor described in Microsoft's Hypervisor Top-Level
//! Functional Specification (TLFS). The behavior in this module is based on
//! version 6.0b of the TLFS, which is available on GitHub:
//! https://github.com/MicrosoftDocs/Virtualization-Documentation/blob/main/tlfs/Hypervisor%20Top%20Level%20Functional%20Specification%20v6.0b.pdf
//!
//! Microsoft also maintains a list of minimum requirements for any hypervisor
//! that intends to implement a Hyper-V-compatible interface:
//! https://github.com/MicrosoftDocs/Virtualization-Documentation/blob/main/tlfs/Requirements%20for%20Implementing%20the%20Microsoft%20Hypervisor%20Interface.pdf

use std::sync::{Arc, Mutex};

use cpuid_utils::{CpuidIdent, CpuidSet, CpuidValues};
use overlay::{OverlayError, OverlayKind, OverlayManager, OverlayPage};

use crate::{
    accessors::MemAccessor,
    common::{Lifecycle, VcpuId},
    enlightenment::{
        hyperv::{bits::*, hypercall::MsrHypercallValue},
        AddCpuidError,
    },
    migrate::{
        MigrateCtx, MigrateSingle, MigrateStateError, Migrator, PayloadOffer,
        PayloadOutput,
    },
    msr::{MsrId, RdmsrOutcome, WrmsrOutcome},
};

mod bits;
mod hypercall;
mod overlay;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn hyperv_wrmsr_guest_os_id(val: u64) {}
    fn hyperv_wrmsr_hypercall(val: u64, gpa: u64, locked: bool, enabled: bool) {
    }
    fn hyperv_wrmsr_hypercall_bad_gpa(gpa: u64) {}
}

const TYPE_NAME: &str = "guest-hyperv-interface";

#[derive(Default)]
struct Inner {
    /// This enlightenment's overlay manager.
    overlay_manager: Arc<OverlayManager>,

    /// The last value stored in the [`bits::HV_X64_MSR_GUEST_OS_ID`] MSR.
    msr_guest_os_id_value: u64,

    /// The last value stored in the [`bits::HV_X64_MSR_HYPERCALL`] MSR.
    msr_hypercall_value: MsrHypercallValue,

    hypercall_overlay: Option<OverlayPage>,
}

pub struct HyperV {
    #[allow(dead_code)]
    log: slog::Logger,
    inner: Mutex<Inner>,
    acc_mem: MemAccessor,
}

impl HyperV {
    /// Creates a new Hyper-V enlightenment stack.
    pub fn new(log: &slog::Logger) -> Self {
        let acc_mem = MemAccessor::new_orphan();
        let log = log.new(slog::o!("component" => "hyperv"));
        Self { log, inner: Mutex::new(Inner::default()), acc_mem }
    }

    /// Handles a write to the HV_X64_MSR_GUEST_OS_ID register.
    fn handle_wrmsr_guest_os_id(&self, value: u64) -> WrmsrOutcome {
        probes::hyperv_wrmsr_guest_os_id!(|| value);
        let mut inner = self.inner.lock().unwrap();

        // TLFS section 3.13 says that the hypercall page "becomes disabled" if
        // the guest OS ID register is cleared after the hypercall register is
        // set. It also specifies that attempts to set the Enabled bit in that
        // register will be ignored if the guest OS ID is zeroed, so handle this
        // case by clearing the hypercall MSR's Enabled bit but otherwise
        // leaving the hypercall page untouched (as would happen if the guest
        // manually cleared this bit).
        if value == 0 {
            inner.msr_hypercall_value.clear_enabled();
            inner.hypercall_overlay.take();
        }

        inner.msr_guest_os_id_value = value;
        WrmsrOutcome::Handled
    }

    /// Handles a write to the HV_X64_MSR_HYPERCALL register. See TLFS section
    /// 3.13 and [`MsrHypercallValue`].
    fn handle_wrmsr_hypercall(&self, value: u64) -> WrmsrOutcome {
        let mut new = MsrHypercallValue(value);
        probes::hyperv_wrmsr_hypercall!(|| (
            value,
            new.gpa().0,
            new.locked(),
            new.enabled()
        ));

        let mut inner = self.inner.lock().unwrap();
        let old = inner.msr_hypercall_value;

        // This MSR is immutable once the Locked bit is set.
        if old.locked() {
            return WrmsrOutcome::Handled;
        }

        // If this MSR is written when no guest OS ID is set, the Enabled bit is
        // cleared and the write succeeds.
        if inner.msr_guest_os_id_value == 0 {
            new.clear_enabled();
        }

        // If the Enabled bit is not set, there's nothing to try to expose to
        // the guest.
        if !new.enabled() {
            inner.msr_hypercall_value = new;
            inner.hypercall_overlay.take();
            return WrmsrOutcome::Handled;
        }

        // Ensure the overlay is in the correct position.
        let res = if let Some(overlay) = inner.hypercall_overlay.as_mut() {
            overlay.move_to(new.gpfn())
        } else {
            inner
                .overlay_manager
                .add_overlay(
                    new.gpfn(),
                    OverlayKind::HypercallReturnNotSupported,
                )
                .map(|overlay| {
                    inner.hypercall_overlay = Some(overlay);
                })
        };

        match res {
            Ok(()) => {
                inner.msr_hypercall_value = new;
                WrmsrOutcome::Handled
            }
            Err(OverlayError::AddressInaccessible(_)) => {
                WrmsrOutcome::GpException
            }
            // There should only ever be one hypercall overlay at a time, and
            // guest memory should be accessible in the context of a VM exit, so
            // (barring some other invariant being violated) adding an overlay
            // should always succeed if the target PFN is valid.
            Err(e) => {
                panic!("unexpected error establishing hypercall overlay: {e}")
            }
        }
    }
}

impl super::Enlightenment for HyperV {
    fn add_cpuid(&self, cpuid: &mut CpuidSet) -> Result<(), AddCpuidError> {
        let mut to_add = CpuidSet::new(cpuid.vendor());

        let mut add_to_set = |id, val| {
            to_add
                .insert(id, val)
                .expect("Hyper-V CPUID values don't conflict");
        };

        add_to_set(CpuidIdent::leaf(0x4000_0001), HYPERV_LEAF_1_VALUES);
        add_to_set(CpuidIdent::leaf(0x4000_0002), HYPERV_LEAF_2_VALUES);
        add_to_set(
            CpuidIdent::leaf(0x4000_0003),
            CpuidValues {
                eax: HyperVLeaf3Eax::default().bits(),
                ..Default::default()
            },
        );

        add_to_set(CpuidIdent::leaf(0x4000_0004), HYPERV_LEAF_4_VALUES);
        add_to_set(CpuidIdent::leaf(0x4000_0005), HYPERV_LEAF_5_VALUES);

        // Set the maximum available CPUID leaf to the smallest value required
        // to expose all of the enlightenment's features.
        //
        // WARNING: In at least some versions of propolis-server, the CPUID
        // configuration generated by this enlightenment is not part of the
        // instance description that the migration source sends to its target.
        // Instead, the source sends the target its *enlightenment
        // configuration* and assumes that the target will produce the same
        // CPUID settings the source produced. This includes the maximum
        // available enlightenment leaf: it should not be set to the maximum
        // leaf this version of Propolis knows about, but to the maximum leaf
        // required by the features enabled in this enlightenment stack.
        add_to_set(
            CpuidIdent::leaf(0x4000_0000),
            bits::hyperv_leaf_0_values(0x4000_0005),
        );

        super::add_cpuid(cpuid, to_add)
    }

    fn rdmsr(&self, vcpu: VcpuId, msr: MsrId) -> RdmsrOutcome {
        match msr.0 {
            HV_X64_MSR_GUEST_OS_ID => RdmsrOutcome::Handled(
                self.inner.lock().unwrap().msr_guest_os_id_value,
            ),
            HV_X64_MSR_HYPERCALL => RdmsrOutcome::Handled(
                self.inner.lock().unwrap().msr_hypercall_value.0,
            ),
            HV_X64_MSR_VP_INDEX => {
                let id: u32 = vcpu.into();
                RdmsrOutcome::Handled(id as u64)
            }
            _ => RdmsrOutcome::NotHandled,
        }
    }

    fn wrmsr(&self, _vcpu: VcpuId, msr: MsrId, value: u64) -> WrmsrOutcome {
        match msr.0 {
            HV_X64_MSR_GUEST_OS_ID => self.handle_wrmsr_guest_os_id(value),
            HV_X64_MSR_HYPERCALL => self.handle_wrmsr_hypercall(value),
            HV_X64_MSR_VP_INDEX => WrmsrOutcome::GpException,
            _ => WrmsrOutcome::NotHandled,
        }
    }

    fn attach(&self, mem_acc: &MemAccessor) {
        mem_acc.adopt(&self.acc_mem, Some(TYPE_NAME.to_owned()));
        let inner = self.inner.lock().unwrap();
        inner.overlay_manager.attach(&self.acc_mem);
    }
}

impl Lifecycle for HyperV {
    fn type_name(&self) -> &'static str {
        TYPE_NAME
    }

    fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();

        // Create a new overlay manager that tracks no pages. The old overlay
        // manager needs to be dropped before any of the overlay pages that
        // referenced it, so explicitly replace the existing manager with a new
        // one (and drop the old one) before default-initializing the rest of
        // the enlightenment's state.
        let new_overlay_manager = Arc::new(OverlayManager::default());
        let _ = std::mem::replace(
            &mut inner.overlay_manager,
            new_overlay_manager.clone(),
        );

        *inner = Inner {
            overlay_manager: new_overlay_manager,
            ..Default::default()
        };

        inner.overlay_manager.attach(&self.acc_mem);
    }

    fn halt(&self) {
        let mut inner = self.inner.lock().unwrap();

        // Create a new overlay manager and drop the reference to the old one.
        // This should be the only active reference to this manager, since all
        // overlay page operations happen during VM exits, and the vCPUs have
        // all quiesced by this point.
        //
        // This ensures that when this object is dropped, any overlay pages it
        // owns can be dropped safely.
        assert_eq!(Arc::strong_count(&inner.overlay_manager), 1);
        inner.overlay_manager = OverlayManager::new();
    }

    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::Single(self)
    }
}

impl MigrateSingle for HyperV {
    fn export(
        &self,
        _ctx: &MigrateCtx,
    ) -> Result<PayloadOutput, MigrateStateError> {
        let inner = self.inner.lock().unwrap();
        Ok(migrate::HyperVEnlightenmentV1 {
            msr_guest_os_id: inner.msr_guest_os_id_value,
            msr_hypercall: inner.msr_hypercall_value.0,
            overlay_originals: inner.overlay_manager.save_original_contents(),
        }
        .into())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let migrate::HyperVEnlightenmentV1 {
            msr_guest_os_id,
            msr_hypercall,
            overlay_originals,
        } = offer.parse()?;
        let mut inner = self.inner.lock().unwrap();

        // Re-establish any overlay pages that are active in the restored MSRs.
        //
        // A well-behaved source should ensure that the hypercall MSR value is
        // within the guest's PA range and that its Enabled bit agrees with the
        // value of the guest OS ID MSR. But this data was received over the
        // wire, so for safety's sake, verify it all and return a migration
        // error if anything is inconsistent.
        let msr_hypercall_value = MsrHypercallValue(msr_hypercall);
        let hypercall_overlay = if msr_hypercall_value.enabled() {
            if msr_guest_os_id == 0 {
                return Err(MigrateStateError::ImportFailed(
                    "hypercall MSR enabled but guest OS ID MSR is 0"
                        .to_string(),
                ));
            }

            match inner.overlay_manager.add_overlay(
                msr_hypercall_value.gpfn(),
                OverlayKind::HypercallReturnNotSupported,
            ) {
                Ok(overlay) => Some(overlay),
                Err(e) => {
                    return Err(MigrateStateError::ImportFailed(format!(
                        "failed to re-establish hypercall overlay: {e}"
                    )))
                }
            }
        } else {
            None
        };

        // Hand the overlay manager the original contents of any guest pages
        // that have active overlays. This needs to be done after all existing
        // overlays are re-established so that the overlay PFNs appear in the
        // manager's tables.
        inner.overlay_manager.restore_original_contents(overlay_originals)?;

        *inner = Inner {
            overlay_manager: inner.overlay_manager.clone(),
            msr_guest_os_id_value: msr_guest_os_id,
            msr_hypercall_value,
            hypercall_overlay,
        };

        Ok(())
    }
}

mod migrate {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    use crate::migrate::{Schema, SchemaId};

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "type", content = "value")]
    pub(super) enum OverlaidGuestPage {
        Zero,
        Page(Vec<u8>),
    }

    impl std::fmt::Debug for OverlaidGuestPage {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Zero => write!(f, "Zero"),
                Self::Page(_) => {
                    f.debug_tuple("Page").field(&"<page redacted>").finish()
                }
            }
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct HyperVEnlightenmentV1 {
        pub(super) msr_guest_os_id: u64,
        pub(super) msr_hypercall: u64,
        pub(super) overlay_originals: BTreeMap<u64, OverlaidGuestPage>,
    }

    impl Schema<'_> for HyperVEnlightenmentV1 {
        fn id() -> SchemaId {
            (super::TYPE_NAME, 1)
        }
    }
}
