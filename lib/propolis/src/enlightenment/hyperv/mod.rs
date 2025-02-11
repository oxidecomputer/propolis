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

use std::sync::{Arc, Mutex, OnceLock};

use cpuid_utils::{CpuidIdent, CpuidSet, CpuidValues};
use overlay::{OverlayError, OverlayKind, OverlayManager, OverlayPage};
use slog::info;

use crate::{
    accessors::MemAccessor,
    common::{Lifecycle, VcpuId},
    enlightenment::{
        hyperv::{
            bits::*,
            hypercall::MsrHypercallValue,
            tsc::{MsrReferenceTscValue, ReferenceTscPage},
        },
        AddCpuidError,
    },
    migrate::{
        MigrateCtx, MigrateSingle, MigrateStateError, Migrator, PayloadOffer,
        PayloadOutput,
    },
    msr::{MsrId, RdmsrOutcome, WrmsrOutcome},
    vmm::{self, VmmHdl},
};

mod bits;
mod hypercall;
mod overlay;
mod tsc;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn hyperv_wrmsr_guest_os_id(val: u64) {}
    fn hyperv_wrmsr_hypercall(val: u64, gpa: u64, locked: bool, enabled: bool) {
    }
    fn hyperv_wrmsr_reference_tsc(val: u64, gpa: u64, enabled: bool) {}
    fn hyperv_wrmsr_hypercall_bad_gpa(gpa: u64) {}
}

const TYPE_NAME: &str = "guest-hyperv-interface";

/// A set of features that can be enabled for a given Hyper-V instance.
#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    /// Enables the reference time MSR and the reference TSC page.
    pub reference_tsc: bool,
}

/// A collection of overlay pages that a Hyper-V enlightenment stack might be
/// managing.
#[derive(Default)]
struct OverlayPages {
    hypercall: Option<OverlayPage>,
    tsc: Option<OverlayPage>,
}

#[derive(Default)]
struct Inner {
    /// This enlightenment's overlay manager.
    overlay_manager: Arc<OverlayManager>,

    /// The last value stored in the [`bits::HV_X64_MSR_GUEST_OS_ID`] MSR.
    msr_guest_os_id_value: u64,

    /// The last value stored in the [`bits::HV_X64_MSR_HYPERCALL`] MSR.
    msr_hypercall_value: MsrHypercallValue,

    /// This enlightenment's active overlay page handles.
    overlays: OverlayPages,

    /// The last value stored in the [`bits::HV_X64_MSR_REFERENCE_TSC`] MSR.
    msr_reference_tsc_value: MsrReferenceTscValue,
}

pub struct HyperV {
    #[allow(dead_code)]
    log: slog::Logger,
    features: Features,
    inner: Mutex<Inner>,
    acc_mem: MemAccessor,
    vmm_hdl: OnceLock<Arc<VmmHdl>>,
}

impl HyperV {
    /// Creates a new Hyper-V enlightenment stack with the supplied `features`.
    pub fn new(log: &slog::Logger, features: Features) -> Self {
        let acc_mem = MemAccessor::new_orphan();
        let log = log.new(slog::o!("component" => "hyperv"));
        info!(
            log,
            "creating Hyper-V enlightenment stack";
            "features" => ?features
        );

        Self {
            log,
            features,
            inner: Mutex::new(Inner::default()),
            acc_mem,
            vmm_hdl: OnceLock::new(),
        }
    }

    /// Returns a reference to this manager's VMM handle.
    ///
    /// # Panics
    ///
    /// Panics if the handle has not been initialized yet, which occurs if this
    /// routine is called before the enlightenment receives its `attach`
    /// callout.
    fn vmm_hdl(&self) -> &Arc<VmmHdl> {
        self.vmm_hdl.get().unwrap()
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
            inner.overlays.hypercall.take();
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
            inner.overlays.hypercall.take();
            return WrmsrOutcome::Handled;
        }

        // Ensure the overlay is in the correct position.
        let res = if let Some(overlay) = inner.overlays.hypercall.as_mut() {
            overlay.move_to(new.gpfn())
        } else {
            inner
                .overlay_manager
                .add_overlay(
                    new.gpfn(),
                    OverlayKind::HypercallReturnNotSupported,
                )
                .map(|overlay| {
                    inner.overlays.hypercall = Some(overlay);
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

    fn handle_rdmsr_time_ref_count(&self) -> RdmsrOutcome {
        if !self.features.reference_tsc {
            return RdmsrOutcome::GpException;
        }

        let time_data = vmm::time::export_time_data(self.vmm_hdl())
            .expect("VMM time data can always be exported");

        // `hrtime` is the current host high-resolution timer value (in
        // nanoseconds), and `boot_hrtime` is the high-resolution timer value at
        // which the VM was started, so it suffices to subtract the latter from
        // the former and divide by 100 to get 100-nanosecond units.
        //
        // If this VM is migrated or otherwise saved/restored, the migration
        // protocol is expected to set `boot_hrtime` for the restored VM to
        // account for any hrtime differences between hosts and any time where
        // the VM was paused.
        let ns_since_boot: u64 = time_data
            .hrtime
            .checked_sub(time_data.boot_hrtime)
            .expect("overflow/underflow while calculating reference uptime")
            .try_into()
            .expect("guest uptime won't roll over");

        RdmsrOutcome::Handled(ns_since_boot / 100)
    }

    fn handle_wrmsr_reference_tsc(&self, value: u64) -> WrmsrOutcome {
        if !self.features.reference_tsc {
            return WrmsrOutcome::GpException;
        }

        let new = MsrReferenceTscValue(value);
        probes::hyperv_wrmsr_reference_tsc!(|| (
            value,
            new.gpa().0,
            new.enabled()
        ));

        // The reference TSC MSR can always be written even if the guest OS ID
        // MSR is 0. TLFS section 12.7.1 specifies that if the selected GPA is
        // invalid, "the reference TSC page will not be accessible to the
        // guest," but the MSR write itself does not #GP.
        let mut inner = self.inner.lock().unwrap();
        if !new.enabled() {
            inner.overlays.tsc.take();
        } else if let Some(mut overlay) = inner.overlays.tsc.take() {
            if overlay.move_to(new.gpfn()).is_ok() {
                inner.overlays.tsc = Some(overlay);
            }
        } else {
            let time_data = vmm::time::export_time_data(self.vmm_hdl())
                .expect("time data can be exported from a VMM handle");

            let page = ReferenceTscPage::new(time_data.guest_freq);
            inner.overlays.tsc = inner
                .overlay_manager
                .add_overlay(new.gpfn(), OverlayKind::ReferenceTsc(page))
                .ok();
        }

        inner.msr_reference_tsc_value = new;
        WrmsrOutcome::Handled
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

        let mut leaf_3_eax = HyperVLeaf3Eax::default();
        if self.features.reference_tsc {
            leaf_3_eax |= HyperVLeaf3Eax::PARTITION_REFERENCE_COUNTER;
            leaf_3_eax |= HyperVLeaf3Eax::PARTITION_REFERENCE_TSC;
        }

        add_to_set(
            CpuidIdent::leaf(0x4000_0003),
            CpuidValues { eax: leaf_3_eax.bits(), ..Default::default() },
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
            HV_X64_MSR_TIME_REF_COUNT => self.handle_rdmsr_time_ref_count(),
            HV_X64_MSR_REFERENCE_TSC => {
                if self.features.reference_tsc {
                    RdmsrOutcome::Handled(
                        self.inner.lock().unwrap().msr_reference_tsc_value.0,
                    )
                } else {
                    RdmsrOutcome::GpException
                }
            }
            _ => RdmsrOutcome::NotHandled,
        }
    }

    fn wrmsr(&self, _vcpu: VcpuId, msr: MsrId, value: u64) -> WrmsrOutcome {
        match msr.0 {
            HV_X64_MSR_GUEST_OS_ID => self.handle_wrmsr_guest_os_id(value),
            HV_X64_MSR_HYPERCALL => self.handle_wrmsr_hypercall(value),
            HV_X64_MSR_REFERENCE_TSC => self.handle_wrmsr_reference_tsc(value),
            HV_X64_MSR_VP_INDEX | HV_X64_MSR_TIME_REF_COUNT => {
                WrmsrOutcome::GpException
            }
            _ => WrmsrOutcome::NotHandled,
        }
    }

    fn attach(&self, mem_acc: &MemAccessor, vmm_hdl: Arc<VmmHdl>) {
        mem_acc.adopt(&self.acc_mem, Some(TYPE_NAME.to_owned()));

        // Using `expect` here requires `VmmHdl` to impl Debug, which isn't
        // really necessary here--the interesting thing is not the contents of
        // the existing handle, but that this routine was called twice to begin
        // with.
        assert!(
            self.vmm_hdl.set(vmm_hdl).is_ok(),
            "Hyper-V enlightenment should only be attached once"
        );

        let inner = self.inner.lock().unwrap();
        inner.overlay_manager.attach(&self.acc_mem);
    }
}

impl Lifecycle for HyperV {
    fn type_name(&self) -> &'static str {
        TYPE_NAME
    }

    fn pause(&self) {
        let mut inner = self.inner.lock().unwrap();

        // Remove all active overlays from service. If the VM migrates, this
        // allows the original guest pages that sit underneath those overlays to
        // be transferred as part of the guest RAM transfer phase instead of
        // possibly being serialized and sent during the device state phase. Any
        // active overlays will be re-established on the target during its
        // device state import phase.
        //
        // Any guest data written to the overlay pages will be lost. That's OK
        // because all the overlays this module currently supports are
        // semantically read-only (guests should expect to take an exception if
        // they try to write to them, although today no such exception is
        // raised).
        //
        // The caller who is coordinating the "pause VM" operation is required
        // to ensure that devices are paused only if vCPUs are paused, so no
        // vCPU will be able to observe the missing overlay.
        inner.overlays = OverlayPages::default();

        assert!(inner.overlay_manager.is_empty());
    }

    fn resume(&self) {
        let mut inner = self.inner.lock().unwrap();

        assert!(inner.overlay_manager.is_empty());

        // Re-establish any overlays that were removed when the enlightenment
        // was paused.
        //
        // Writes to the hypercall MSR only persist if they specify a valid
        // overlay PFN, so adding the hypercall overlay is guaranteed to
        // succeed.
        let hypercall_overlay =
            inner.msr_hypercall_value.enabled().then(|| {
                inner
                    .overlay_manager
                    .add_overlay(
                        inner.msr_hypercall_value.gpfn(),
                        OverlayKind::HypercallReturnNotSupported,
                    )
                    .expect("hypercall MSR is only enabled with a valid PFN")
            });

        let tsc_overlay = inner
            .msr_reference_tsc_value
            .enabled()
            .then(|| {
                // TODO(gjc): this is not correct since we might have migrated in;
                // instead the time data needs to be established at enlightenment
                // setup time
                let time_data = vmm::time::export_time_data(self.vmm_hdl())
                    .expect("time data can be exported from a VMM handle");

                let page = ReferenceTscPage::new(time_data.guest_freq);
                inner
                    .overlay_manager
                    .add_overlay(
                        inner.msr_reference_tsc_value.gpfn(),
                        OverlayKind::ReferenceTsc(page),
                    )
                    .ok()
            })
            .flatten();

        inner.overlays =
            OverlayPages { hypercall: hypercall_overlay, tsc: tsc_overlay };
    }

    fn reset(&self) {
        let inner = self.inner.lock().unwrap();
        assert!(inner.overlay_manager.is_empty());
    }

    fn halt(&self) {
        let inner = self.inner.lock().unwrap();
        assert!(inner.overlay_manager.is_empty());
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
            msr_reference_tsc: inner.msr_reference_tsc_value.0,
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
            msr_reference_tsc,
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

        let msr_reference_tsc_value = MsrReferenceTscValue(msr_reference_tsc);
        let tsc_overlay = msr_reference_tsc_value
            .enabled()
            .then(|| {
                // TODO(gjc): this is not correct; the page config needs to come
                // from the source
                let time_data = vmm::time::export_time_data(self.vmm_hdl())
                    .expect("time data can be exported from a VMM handle");

                let page = ReferenceTscPage::new(time_data.guest_freq);
                inner
                    .overlay_manager
                    .add_overlay(
                        inner.msr_reference_tsc_value.gpfn(),
                        OverlayKind::ReferenceTsc(page),
                    )
                    .ok()
            })
            .flatten();

        *inner = Inner {
            overlay_manager: inner.overlay_manager.clone(),
            msr_guest_os_id_value: msr_guest_os_id,
            msr_hypercall_value,
            msr_reference_tsc_value,
            overlays: OverlayPages {
                hypercall: hypercall_overlay,
                tsc: tsc_overlay,
            },
        };
        Ok(())
    }
}

mod migrate {
    use serde::{Deserialize, Serialize};

    use crate::migrate::{Schema, SchemaId};

    #[derive(Debug, Serialize, Deserialize)]
    pub struct HyperVEnlightenmentV1 {
        pub(super) msr_guest_os_id: u64,
        pub(super) msr_hypercall: u64,
        pub(super) msr_reference_tsc: u64,
    }

    impl Schema<'_> for HyperVEnlightenmentV1 {
        fn id() -> SchemaId {
            (super::TYPE_NAME, 1)
        }
    }
}
