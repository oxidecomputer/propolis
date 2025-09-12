// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for Microsoft Hyper-V emulation.
//!
//! Windows guests and many Linux guests can interoperate with hypervisors that
//! implement the hypervisor described in Microsoft's Hypervisor Top-Level
//! Functional Specification (TLFS). The behavior in this module is based on
//! version 6.0b of the TLFS, which is available on GitHub:
//! <https://github.com/MicrosoftDocs/Virtualization-Documentation/blob/main/tlfs/Hypervisor%20Top%20Level%20Functional%20Specification%20v6.0b.pdf>
//!
//! Microsoft also maintains a list of minimum requirements for any hypervisor
//! that intends to implement a Hyper-V-compatible interface:
//! <https://github.com/MicrosoftDocs/Virtualization-Documentation/blob/main/tlfs/Requirements%20for%20Implementing%20the%20Microsoft%20Hypervisor%20Interface.pdf>

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
            tsc::{MsrReferenceTscValue, ReferenceTsc},
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
    fn hyperv_rdmsr_reference_time(time_units: u64) {}
}

const TYPE_NAME: &str = "guest-hyperv-interface";

/// A set of features that can be enabled for a given Hyper-V instance.
#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    /// Enables the reference time MSR and the reference TSC page.
    pub reference_tsc: bool,
}

/// Wrapper around a hypercall overlay page.
struct HypercallOverlay(OverlayPage);

/// Wrapper around a TSC overlay page.
struct TscOverlay(OverlayPage);

/// A collection of overlay pages that a Hyper-V enlightenment stack might be
/// managing.
#[derive(Default)]
struct OverlayPages {
    hypercall: Option<HypercallOverlay>,
    tsc: Option<TscOverlay>,
}

struct Inner {
    /// This enlightenment's overlay manager.
    overlay_manager: Arc<OverlayManager>,

    /// The last value stored in the [`bits::HV_X64_MSR_GUEST_OS_ID`] MSR.
    msr_guest_os_id_value: u64,

    /// The last value stored in the [`bits::HV_X64_MSR_HYPERCALL`] MSR.
    msr_hypercall_value: MsrHypercallValue,

    /// The state of this stack's reference TSC enlightenment.
    reference_tsc: ReferenceTsc,

    /// This enlightenment's active overlay page handles.
    overlays: OverlayPages,
}

impl Inner {
    fn new(features: &Features) -> Self {
        Self {
            overlay_manager: Arc::default(),
            msr_guest_os_id_value: 0,
            msr_hypercall_value: MsrHypercallValue::default(),
            reference_tsc: if features.reference_tsc {
                ReferenceTsc::Uninitialized
            } else {
                ReferenceTsc::Disabled
            },
            overlays: OverlayPages::default(),
        }
    }

    /// Resets this enlightenment block's volatile values (e.g. MSR values) to
    /// their initial values.
    fn reset(&mut self) {
        *self = Self {
            overlay_manager: self.overlay_manager.clone(),
            msr_guest_os_id_value: 0,
            msr_hypercall_value: MsrHypercallValue::default(),
            reference_tsc: match &self.reference_tsc {
                ReferenceTsc::Enabled { guest_freq, .. } => {
                    ReferenceTsc::Enabled {
                        guest_freq: *guest_freq,
                        msr_value: MsrReferenceTscValue::default(),
                    }
                }
                tsc => *tsc,
            },
            overlays: OverlayPages::default(),
        }
    }

    fn handle_rdmsr_reference_tsc(&self) -> RdmsrOutcome {
        match self.reference_tsc {
            ReferenceTsc::Disabled => RdmsrOutcome::GpException,
            // Well-behaved users of the enlightenment shouldn't allow vCPUs to
            // start dispatching calls to it until the enlightenment is fully
            // initialized.
            ReferenceTsc::Uninitialized => {
                panic!(
                    "reference TSC read from uninitialized enlightenment \
                    (perhaps vCPUs were started without calling attach()?)"
                )
            }
            ReferenceTsc::Enabled { msr_value, .. } => {
                RdmsrOutcome::Handled(msr_value.0)
            }
        }
    }

    fn handle_wrmsr_reference_tsc(&mut self, value: u64) -> WrmsrOutcome {
        if !self.reference_tsc.is_present() {
            return WrmsrOutcome::GpException;
        }

        let new = MsrReferenceTscValue(value);
        probes::hyperv_wrmsr_reference_tsc!(|| (
            value,
            new.gpa().0,
            new.enabled()
        ));

        // Unlike the hypercall MSR, writes to the reference TSC MSR always
        // succeed without raising an exception, even if they try to enable the
        // TSC overlay page at an invalid PFN. See TLFS section 12.7.1.
        let old_overlay = self.overlays.tsc.take();
        self.reference_tsc.set_msr_value(new);
        self.overlays.tsc = if new.enabled() {
            if let Some(mut overlay) = old_overlay {
                overlay.0.move_to(new.gpfn()).ok().map(|_| overlay)
            } else {
                self.reference_tsc.create_overlay(&self.overlay_manager)
            }
        } else {
            None
        };

        WrmsrOutcome::Handled
    }
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
    ///
    /// The caller must call [`attach`] to finish initializing this
    /// enlightenment stack before starting any VM components that depend on it.
    /// Otherwise the stack may panic while the VM is running.
    ///
    /// [`attach`]: super::Enlightenment::attach
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
            inner: Mutex::new(Inner::new(&features)),
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
        self.vmm_hdl.get().expect(
            "a fully-initialized Hyper-V enlightenment always has a \
            VMM handle (did the library user remember to call `attach`?)",
        )
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
            overlay.0.move_to(new.gpfn())
        } else {
            inner
                .overlay_manager
                .add_overlay(
                    new.gpfn(),
                    OverlayKind::HypercallReturnNotSupported,
                )
                .map(|overlay| {
                    inner.overlays.hypercall = Some(HypercallOverlay(overlay));
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

    /// Handles a read of the `HV_X64_MSR_TIME_REF_COUNT` register. See TLFS
    /// section 12.4.
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
        self.inner.lock().unwrap().handle_wrmsr_reference_tsc(value)
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
                self.inner.lock().unwrap().handle_rdmsr_reference_tsc()
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

        let mut inner = self.inner.lock().unwrap();
        inner.overlay_manager.attach(&self.acc_mem);

        if let ReferenceTsc::Uninitialized = inner.reference_tsc {
            let time_data = vmm::time::export_time_data(&vmm_hdl)
                .expect("VMM time data is accessible during attach");

            // N.B. This guest TSC frequency may be overwritten by a future
            // request to import state from a migration source. This is
            // intentional; the migration protocol will configure the kernel VMM
            // to apply hardware TSC scaling so that the guest observes the
            // imported frequency.
            inner.reference_tsc = ReferenceTsc::Enabled {
                guest_freq: time_data.guest_freq,
                msr_value: MsrReferenceTscValue::default(),
            }
        }

        // `attach` should only called once on each enlightenment instance.
        // `VmmHdl` doesn't implement `Debug`, so it's not possible to use
        // `unwrap` or `expect` here.
        assert!(
            self.vmm_hdl.set(vmm_hdl).is_ok(),
            "Enlightenment::attach should be called exactly once per stack"
        );
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
        let hypercall_overlay = inner
            .msr_hypercall_value
            .enabled()
            .then(|| {
                inner
                    .overlay_manager
                    .add_overlay(
                        inner.msr_hypercall_value.gpfn(),
                        OverlayKind::HypercallReturnNotSupported,
                    )
                    .expect("hypercall MSR is only enabled with a valid PFN")
            })
            .map(HypercallOverlay);

        let tsc_overlay = inner
            .reference_tsc
            .is_present()
            .then(|| inner.reference_tsc.create_overlay(&inner.overlay_manager))
            .flatten();

        inner.overlays =
            OverlayPages { hypercall: hypercall_overlay, tsc: tsc_overlay };
    }

    fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();

        // The overlay manager shouldn't have any active overlays, because
        // `pause` drops them all, and state drivers are required to call
        // `pause` before `reset`.
        assert!(inner.overlay_manager.is_empty());

        inner.reset();
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
            reference_tsc: match inner.reference_tsc {
                ReferenceTsc::Disabled => None,
                ReferenceTsc::Uninitialized => {
                    return Err(MigrateStateError::NotReadyForExport);
                }
                ReferenceTsc::Enabled { msr_value, guest_freq } => {
                    Some(migrate::ReferenceTscV1 {
                        msr_value: msr_value.0,
                        guest_freq,
                    })
                }
            },
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
            reference_tsc,
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
                Ok(overlay) => Some(HypercallOverlay(overlay)),
                Err(e) => {
                    return Err(MigrateStateError::ImportFailed(format!(
                        "failed to re-establish hypercall overlay: {e}"
                    )))
                }
            }
        } else {
            None
        };

        let (reference_tsc, tsc_overlay) = if let Some(imported_tsc) =
            reference_tsc
        {
            if !inner.reference_tsc.is_present() {
                return Err(MigrateStateError::ImportFailed(
                    "imported payload has reference TSC data, but that \
                        enlightenment is disabled"
                        .to_string(),
                ));
            }

            // Ensure that the TSC overlay exists and that it exposes the
            // correct scaling factor for the guest's nominal TSC frequency.
            // This may be different from the default scaling factor that was
            // read from the kernel VMM when the enlightenment stack was
            // initialized.
            let reference_tsc = ReferenceTsc::Enabled {
                msr_value: MsrReferenceTscValue(imported_tsc.msr_value),
                guest_freq: imported_tsc.guest_freq,
            };

            let overlay = reference_tsc.create_overlay(&inner.overlay_manager);
            (reference_tsc, overlay)
        } else {
            if inner.reference_tsc.is_present() {
                return Err(MigrateStateError::ImportFailed(
                    "imported payload has no reference TSC data, but that \
                        enlightenment is enabled"
                        .to_string(),
                ));
            }

            (ReferenceTsc::Disabled, None)
        };

        *inner = Inner {
            overlay_manager: inner.overlay_manager.clone(),
            msr_guest_os_id_value: msr_guest_os_id,
            msr_hypercall_value,
            reference_tsc,
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

    /// Reference TSC enlightenment state.
    #[derive(Debug, Serialize, Deserialize)]
    pub struct ReferenceTscV1 {
        /// The value of the `HV_X64_MSR_REFERENCE_TSC` MSR.
        pub(super) msr_value: u64,

        /// The nominal TSC frequency for this VM. This is established when a VM
        /// first boots and determines the TSC scaling factor that's written to
        /// its reference TSC page.
        ///
        /// This module assumes that the guest's observed TSC frequency is
        /// invariant: when a VM migrates, the migrator is required to take
        /// steps to ensure that the guest TSC frequency on the target is the
        /// same as on the source. Migrators can use the
        /// [`crate::vmm::time::adjust_time_data`] function to compute the
        /// appropriate scaling factors to pass to bhyve to achieve this.
        pub(super) guest_freq: u64,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct HyperVEnlightenmentV1 {
        pub(super) msr_guest_os_id: u64,
        pub(super) msr_hypercall: u64,
        pub(super) reference_tsc: Option<ReferenceTscV1>,
    }

    impl Schema<'_> for HyperVEnlightenmentV1 {
        fn id() -> SchemaId {
            (super::TYPE_NAME, 1)
        }
    }
}
