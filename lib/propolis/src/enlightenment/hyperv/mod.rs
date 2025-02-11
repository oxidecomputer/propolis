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

use std::sync::Mutex;

use cpuid_utils::{CpuidIdent, CpuidSet, CpuidValues};

use crate::{
    accessors::MemAccessor,
    common::{GuestRegion, Lifecycle, VcpuId, PAGE_SIZE},
    enlightenment::{
        hyperv::{
            bits::*,
            hypercall::{hypercall_page_contents, MsrHypercallValue},
        },
        AddCpuidError,
    },
    migrate::{
        MigrateCtx, MigrateSingle, MigrateStateError, Migrator, PayloadOffer,
        PayloadOutput,
    },
    msr::{MsrId, RdmsrOutcome, WrmsrOutcome},
    vmm::SubMapping,
};

mod bits;
mod hypercall;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn hyperv_wrmsr_guest_os_id(val: u64) {}
    fn hyperv_wrmsr_hypercall(val: u64, gpa: u64, locked: bool, enabled: bool) {
    }
    fn hyperv_wrmsr_hypercall_bad_gpa(gpa: u64) {}
}

const TYPE_NAME: &str = "guest-hyperv-interface";

#[derive(Debug, Default)]
struct Inner {
    /// The last value stored in the [`bits::HV_X64_MSR_GUEST_OS_ID`] MSR.
    msr_guest_os_id_value: u64,

    /// The last value stored in the [`bits::HV_X64_MSR_HYPERCALL`] MSR.
    msr_hypercall_value: MsrHypercallValue,
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
            return WrmsrOutcome::Handled;
        }

        let memctx = self
            .acc_mem
            .access()
            .expect("guest memory is always accessible during wrmsr");

        let region = GuestRegion(new.gpa(), PAGE_SIZE);

        // Mapping will fail if the requested GPA is out of the guest's physical
        // address range. The TLFS specifies that this should raise #GP.
        let Some(mapping) = memctx.writable_region(&region) else {
            probes::hyperv_wrmsr_hypercall_bad_gpa!(|| new.gpa().0);
            return WrmsrOutcome::GpException;
        };

        // Write the hypercall instruction sequence to the requested GPA.
        //
        // TODO: TLFS section 5.2.1 specifies that when an overlay is removed,
        // "the underlying GPA page is 'uncovered', and an existing mapping
        // becomes accessible to the guest." Empirically, at least some other
        // Hv#1 implementations don't appear to follow this rule, and most
        // common guest OSes don't rely on being able to disable or remove the
        // hypercall page. Nevertheless, Propolis should eventually follow this
        // rule.
        write_overlay_page(&mapping, &hypercall_page_contents());

        inner.msr_hypercall_value = new;
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
    }
}

fn write_overlay_page(mapping: &SubMapping<'_>, contents: &[u8; PAGE_SIZE]) {
    let written = mapping
        .write_bytes(contents)
        .expect("overlay pages are always writable");

    assert_eq!(written, PAGE_SIZE, "overlay pages can be written completely");
}

impl Lifecycle for HyperV {
    fn type_name(&self) -> &'static str {
        TYPE_NAME
    }

    fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        *inner = Inner::default();
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
        }
        .into())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let data: migrate::HyperVEnlightenmentV1 = offer.parse()?;

        // A well-behaved source should ensure that the hypercall MSR value is
        // within the guest's PA range and that its Enabled bit agrees with the
        // value of the guest OS ID MSR. But this data was received over the
        // wire, so for safety's sake, verify it all and return a migration
        // error if anything is inconsistent.
        let hypercall_msr = MsrHypercallValue(data.msr_hypercall);
        if hypercall_msr.enabled() {
            if data.msr_guest_os_id == 0 {
                return Err(MigrateStateError::ImportFailed(
                    "hypercall MSR enabled but guest OS ID MSR is 0"
                        .to_string(),
                ));
            }

            let Some(mapping) = ctx
                .mem
                .writable_region(&GuestRegion(hypercall_msr.gpa(), PAGE_SIZE))
            else {
                return Err(MigrateStateError::ImportFailed(format!(
                    "couldn't map hypercall page for MSR value \
                    {hypercall_msr:?}"
                )));
            };

            write_overlay_page(&mapping, &hypercall_page_contents());
        }

        let mut inner = self.inner.lock().unwrap();
        inner.msr_guest_os_id_value = data.msr_guest_os_id;
        inner.msr_hypercall_value = hypercall_msr;
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
    }

    impl Schema<'_> for HyperVEnlightenmentV1 {
        fn id() -> SchemaId {
            (super::TYPE_NAME, 1)
        }
    }
}
