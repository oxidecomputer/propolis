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
use overlay::{
    OverlayContents, OverlayError, OverlayKind, OverlayManager, OverlayPage,
};
use slog::info;

use crate::{
    accessors::MemAccessor,
    common::{Lifecycle, VcpuId},
    enlightenment::{
        hyperv::{
            bits::*,
            hypercall::{hypercall_page_contents, MsrHypercallValue},
            tsc::{MsrReferenceTscValue, ReferenceTscPage},
        },
        AddCpuidError,
    },
    migrate::{
        MigrateCtx, MigrateMulti, MigrateStateError, Migrator, PayloadOffers,
        PayloadOutputs,
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
    fn hyperv_rdmsr_reference_time(time_units: u64) {}
}

const TYPE_NAME: &str = "guest-hyperv-interface";

/// A set of features that can be enabled for a given Hyper-V instance.
#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    /// Enables the reference time MSR and the reference TSC page.
    pub reference_tsc: bool,
}

#[derive(Default)]
struct Inner {
    /// This enlightenment's overlay manager.
    overlay_manager: Arc<OverlayManager>,

    /// The last value stored in the [`bits::HV_X64_MSR_GUEST_OS_ID`] MSR.
    msr_guest_os_id_value: u64,

    /// The last value stored in the [`bits::HV_X64_MSR_HYPERCALL`] MSR.
    msr_hypercall_value: MsrHypercallValue,
    hypercall_overlay: Option<OverlayPage>,

    /// The last value stored in the [`bits::HV_X64_MSR_REFERENCE_TSC`] MSR.
    msr_reference_tsc_value: MsrReferenceTscValue,
    reference_tsc_overlay: Option<OverlayPage>,
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
                    OverlayKind::Hypercall,
                    OverlayContents(Box::new(hypercall_page_contents())),
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

    fn handle_rdmsr_time_ref_count(&self) -> RdmsrOutcome {
        if !self.features.reference_tsc {
            return RdmsrOutcome::GpException;
        }

        let time_data = vmm::time::export_time_data(self.vmm_hdl())
            .expect("VMM time data can always be exported");

        // Two fields in the `time_data` struct are relevant here:
        //
        // - `hrtime` is the time since the host booted, in nanoseconds.
        // - `boot_hrtime` is the host time at which the VM booted.
        //
        // `boot_hrtime` is allowed to be negative if the VM started before
        // its current host did. This can happen if the VM migrated to this host
        // after being started on some other (even longer-lived) host.
        //
        // Validate a couple of assumptions:
        //
        // - The host never reports a negative uptime. (Note that i64::MAX
        //   nanoseconds is 9.2e18 ns, so it takes approximately 292 years for
        //   a nanosecond uptime counter to wrap.)
        // - The guest's boot time is never in the future, i.e., it is never
        //   greater than the current host time. If this happens, it either
        //   means that host time went backwards or that the guest's
        //   `boot_hrtime` was incorrectly mutated. In either case, this
        //   computation is going to produce an incorrect guest timestamp value.
        //
        // These cases are both unexpected, so if either occurs, just crash the
        // VM rather than make the guest deal (perhaps badly, e.g. by persisting
        // an invalid calculated wall-clock time to disk) with reference time
        // going backwards or with large skips in reference time.
        //
        // Note that during a live migration, the migration protocol is expected
        // to verify these conditions and fail migration if creating either of
        // them is required to represent guest time accurately.
        assert!(time_data.hrtime >= 0);
        assert!(time_data.hrtime >= time_data.boot_hrtime);

        // Since hrtime is non-negative, this subtraction should never
        // underflow, but it can *overflow* if `boot_hrtime` is negative and of
        // sufficient magnitude.
        //
        // Although this situation could be represented by trying to wrap the
        // reference counter, it's simpler just to abort, since this implies a
        // VM uptime of more than 292 years. (If you are dealing with this
        // problem from the 24th century, please accept the present author's
        // apologies!)
        let guest_uptime = time_data
            .hrtime
            .checked_sub(time_data.boot_hrtime)
            .expect("overflow while calculating reference uptime");

        // Since hrtime >= boot_hrtime, the resulting guest uptime should always
        // be non-negative, and so it should be trivial to represent it as a
        // u64.
        let guest_uptime: u64 = guest_uptime
            .try_into()
            .expect("boot_hrtime should be less than host hrtime");

        // The computed uptime is in nanoseconds, but reference time is measured
        // in 100 ns units.
        let reference_uptime = guest_uptime / 100;

        probes::hyperv_rdmsr_reference_time!(|| reference_uptime);
        RdmsrOutcome::Handled(reference_uptime)
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
        // guest," but the MSR write itself does not #GP. It therefore suffices
        // here just to check if the guest requested an overlay and, if it did,
        // attempt to establish one there, either by moving the previous overlay
        // (if there is one) or by creating a new one.
        let mut inner = self.inner.lock().unwrap();
        let old_overlay = inner.reference_tsc_overlay.take();
        inner.reference_tsc_overlay = if new.enabled() {
            if let Some(mut overlay) = old_overlay {
                overlay.move_to(new.gpfn()).ok().map(|_| overlay)
            } else {
                let time_data = vmm::time::export_time_data(self.vmm_hdl())
                    .expect("time data can be exported from a VMM handle");

                let page = ReferenceTscPage::new(time_data.guest_freq);
                inner
                    .overlay_manager
                    .add_overlay(
                        new.gpfn(),
                        OverlayKind::ReferenceTsc,
                        OverlayContents(page.into()),
                    )
                    .ok()
            }
        } else {
            None
        };

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
        Migrator::Multi(self)
    }
}

impl MigrateMulti for HyperV {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let inner = self.inner.lock().unwrap();
        output.push(
            migrate::HyperVEnlightenmentV1 {
                msr_guest_os_id: inner.msr_guest_os_id_value,
                msr_hypercall: inner.msr_hypercall_value.0,
                msr_reference_tsc: inner.msr_reference_tsc_value.0,
            }
            .into(),
        )?;

        output.push(inner.overlay_manager.export().into())?;
        Ok(())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let data: migrate::HyperVEnlightenmentV1 = offer.take()?;
        let overlays: overlay::migrate::HyperVOverlaysV1 = offer.take()?;

        let mut inner = self.inner.lock().unwrap();
        let mut overlays = inner.overlay_manager.import(overlays)?;

        let msr_hypercall = MsrHypercallValue(data.msr_hypercall);
        let hypercall_overlay = overlays
            .iter()
            .position(|page| page.kind() == OverlayKind::Hypercall)
            .map(|pos| overlays.swap_remove(pos));

        if msr_hypercall.enabled() {
            if data.msr_guest_os_id == 0 {
                return Err(MigrateStateError::ImportFailed(
                    "hypercall page enabled but guest OS ID MSR is 0"
                        .to_string(),
                ));
            }

            let Some(overlay) = &hypercall_overlay else {
                return Err(MigrateStateError::ImportFailed(
                    "hypercall page enabled but no overlay was imported"
                        .to_string(),
                ));
            };

            if overlay.pfn() != msr_hypercall.gpfn() {
                return Err(MigrateStateError::ImportFailed(format!(
                    "hypercall MSR has PFN {:x} but overlay has {:x}",
                    msr_hypercall.gpfn(),
                    overlay.pfn()
                )));
            }
        } else {
            if hypercall_overlay.is_some() {
                return Err(MigrateStateError::ImportFailed(
                    "hypercall overlay present but page is disabled"
                        .to_string(),
                ));
            }
        }

        // This enlightenment assumes that if a VM is exported and imported, the
        // caller asking to import will configure the bhyve VM so that it has
        // the same apparent guest frequency and offset the VM had when it was
        // exported. Thus, it's not necessary to change the reference TSC page's
        // contents here. See the module comment in tsc.rs for more details.
        let msr_reference_tsc = MsrReferenceTscValue(data.msr_reference_tsc);
        let tsc_overlay = overlays
            .iter()
            .position(|page| page.kind() == OverlayKind::ReferenceTsc)
            .map(|pos| overlays.swap_remove(pos));

        if msr_reference_tsc.enabled() {
            let pfn_is_valid = inner
                .overlay_manager
                .pfn_is_valid(msr_reference_tsc.gpfn())
                .expect("guest memory is accessible during import");

            if pfn_is_valid {
                let Some(overlay) = &tsc_overlay else {
                    return Err(MigrateStateError::ImportFailed(
                        "reference TSC page enabled but no overlay was imported"
                            .to_string(),
                    ));
                };

                if overlay.pfn() != msr_reference_tsc.gpfn() {
                    return Err(MigrateStateError::ImportFailed(format!(
                        "reference TSC MSR has PFN {:x} but overlay has {:x}",
                        msr_reference_tsc.gpfn(),
                        overlay.pfn()
                    )));
                };
            } else {
                if tsc_overlay.is_some() {
                    return Err(MigrateStateError::ImportFailed(format!(
                        "TSC overlay exists at pfn {:x}, which is not a valid \
                        overlay PFN",
                        msr_reference_tsc.gpfn()
                    )));
                }
            }
        } else {
            if tsc_overlay.is_some() {
                return Err(MigrateStateError::ImportFailed(
                    "reference TSC overlay present but page is disabled"
                        .to_string(),
                ));
            }
        }

        if !overlays.is_empty() {
            return Err(MigrateStateError::ImportFailed(format!(
                "unexpected overlay pages: {:?}",
                overlays
            )));
        }

        *inner = Inner {
            overlay_manager: inner.overlay_manager.clone(),
            msr_guest_os_id_value: data.msr_guest_os_id,
            msr_hypercall_value: msr_hypercall,
            msr_reference_tsc_value: msr_reference_tsc,
            hypercall_overlay,
            reference_tsc_overlay: tsc_overlay,
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
