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

use bits::*;
use cpuid_utils::{CpuidIdent, CpuidSet, CpuidValues};
use hypercall::{hypercall_page_contents, MsrHypercallValue};
use slog::info;

use crate::{
    accessors::MemAccessor,
    common::{GuestRegion, Lifecycle, VcpuId, PAGE_SIZE},
    enlightenment::AddCpuidError,
    migrate::Migrator,
    msr::{MsrId, RdmsrOutcome, WrmsrOutcome},
    vmm::SubMapping,
};

mod bits;
mod hypercall;

const TYPE_NAME: &str = "guest-hyperv-interface";

#[derive(Debug, Default)]
struct Inner {
    /// The last value stored in the [`bits::MSR_GUEST_OS_ID`] MSR.
    msr_guest_os_id_value: u64,

    /// The last value stored in the [`bits::MSR_HYPERCALL`] MSR.
    msr_hypercall_value: MsrHypercallValue,
}

pub struct HyperV {
    log: slog::Logger,
    inner: Mutex<Inner>,
    acc_mem: MemAccessor,
}

impl HyperV {
    pub fn new(log: slog::Logger) -> Self {
        let acc_mem = MemAccessor::new_orphan();
        Self { log, inner: Mutex::new(Inner::default()), acc_mem }
    }

    fn handle_wrmsr_guest_os_id(&self, value: u64) -> WrmsrOutcome {
        let mut inner = self.inner.lock().unwrap();

        // TLFS section 3.13 specifies that if the guest OS ID register is
        // cleared, then the hypercall page immediately "becomes disabled." The
        // exact semantics of "becoming disabled" are unclear, but in context it
        // seems most reasonable to read this as "the Enabled bit is cleared and
        // the hypercall overlay is removed."
        if value == 0 {
            info!(&self.log, "guest cleared HV_X64_MSR_GUEST_OS_ID");
            inner.msr_hypercall_value.clear_enabled();
        }

        inner.msr_guest_os_id_value = value;
        WrmsrOutcome::Handled
    }

    /// Handles a write to the HV_X64_MSR_HYPERCALL register. See TLFS section
    /// 3.13.
    fn handle_wrmsr_hypercall(&self, value: u64) -> WrmsrOutcome {
        let mut inner = self.inner.lock().unwrap();
        let old = inner.msr_hypercall_value;

        // TLFS section 3.13 says that this MSR is "immutable" once the locked
        // bit is set.
        if old.locked() {
            return WrmsrOutcome::Handled;
        }

        // If this MSR is written when no guest OS ID is set, the Enabled bit is
        // cleared and the write succeeds.
        let mut new = MsrHypercallValue(value);
        if inner.msr_guest_os_id_value == 0 {
            new.clear_enabled();
        }

        // If the Enabled bit is set, expose the hypercall instruction sequence
        // at the requested GPA. The TLFS specifies that this raises #GP if the
        // selected physical address is outside the bounds of the guest's
        // physical address space.
        //
        // The TLFS describes this page as an "overlay" that "covers whatever
        // else is mapped to the GPA range." The spec is ambiguous as to whether
        // this means that the page's previous contents must be restored if the
        // page is disabled or its address later changes. Empirically, most
        // common guest OSes don't appear to move the page after creating it,
        // and at least some other hypervisors don't bother with saving and
        // restoring the old page's contents. So, for simplicity, simply write
        // the hypercall instruction sequence to the requested page and call it
        // a day.
        let outcome = if new.enabled() {
            let memctx = self
                .acc_mem
                .access()
                .expect("guest memory is always accessible during wrmsr");

            let region = GuestRegion(new.gpa(), PAGE_SIZE);
            if let Some(mapping) = memctx.writable_region(&region) {
                write_overlay_page(&mapping, &hypercall_page_contents());
                WrmsrOutcome::Handled
            } else {
                WrmsrOutcome::GpException
            }
        } else {
            WrmsrOutcome::Handled
        };

        // Commit the new MSR value if the write was handled.
        if outcome == WrmsrOutcome::Handled {
            inner.msr_hypercall_value = new;
        }

        outcome
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

        add_to_set(CpuidIdent::leaf(0x4000_0000), HYPERV_LEAF_0_VALUES);
        add_to_set(CpuidIdent::leaf(0x4000_0001), HYPERV_LEAF_1_VALUES);
        add_to_set(CpuidIdent::leaf(0x4000_0002), HYPERV_LEAF_2_VALUES);
        add_to_set(CpuidIdent::leaf(0x4000_0004), HYPERV_LEAF_4_VALUES);
        add_to_set(CpuidIdent::leaf(0x4000_0005), HYPERV_LEAF_5_VALUES);
        add_to_set(CpuidIdent::leaf(0x4000_0006), HYPERV_LEAF_6_VALUES);
        add_to_set(CpuidIdent::leaf(0x4000_0007), HYPERV_LEAF_7_VALUES);
        add_to_set(CpuidIdent::leaf(0x4000_0008), HYPERV_LEAF_8_VALUES);
        add_to_set(CpuidIdent::leaf(0x4000_0009), HYPERV_LEAF_9_VALUES);
        add_to_set(CpuidIdent::leaf(0x4000_000A), HYPERV_LEAF_A_VALUES);

        let leaf_3_eax = HyperVLeaf3Eax::HYPERCALL | HyperVLeaf3Eax::VP_INDEX;

        add_to_set(
            CpuidIdent::leaf(0x4000_0003),
            CpuidValues { eax: leaf_3_eax.bits(), ..Default::default() },
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

        // The contents of guest memory are going to be completely reinitialized
        // anyway, so there's no need to manually remove any overlay pages.
        *inner = Inner::default();
    }

    // TODO: Migration support.
    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::NonMigratable
    }
}
