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
use overlay::OverlayPage;
use slog::{info, warn};

use crate::{
    accessors::MemAccessor,
    common::{Lifecycle, VcpuId},
    enlightenment::AddCpuidError,
    migrate::Migrator,
    msr::{MsrId, RdmsrOutcome, WrmsrOutcome},
};

mod bits;
mod hypercall;
mod overlay;

const TYPE_NAME: &str = "guest-hyperv-interface";

#[derive(Default)]
struct Inner {
    /// The last value stored in the [`bits::MSR_GUEST_OS_ID`] MSR.
    msr_guest_os_id_value: u64,

    /// The last value stored in the [`bits::MSR_HYPERCALL`] MSR.
    msr_hypercall_value: MsrHypercallValue,

    /// The previous contents of the guest physical page that has been overlaid
    /// with the hypercall page.
    hypercall_overlay: Option<OverlayPage>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("msr_guest_os_id_value", &self.msr_guest_os_id_value)
            .field("msr_hypercall_value", &self.msr_hypercall_value)
            .field(
                "old_hypercall_page_contents.is_some()",
                &self.hypercall_overlay.is_some(),
            )
            .finish()
    }
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

        // TODO(gjc): TLFS section 3.13 says that if a guest clears the guest OS ID
        // register, the hypercall page "become[s] disabled." It's unclear
        // whether this means just that the Enabled bit in the relevant register
        // is cleared or whether it also means that the hypercall page overlay
        // is removed.
        if value == 0 {
            warn!(&self.log, "guest cleared HV_X64_MSR_GUEST_OS_ID");
        }

        inner.msr_guest_os_id_value = value;
        WrmsrOutcome::Handled
    }

    fn handle_wrmsr_hypercall(&self, value: u64) -> WrmsrOutcome {
        let mut inner = self.inner.lock().unwrap();

        // If the previous value of the hypercall register has the lock bit set,
        // ignore future modifications.
        let old = inner.msr_hypercall_value;
        if old.locked() {
            return WrmsrOutcome::Handled;
        }

        let new = MsrHypercallValue(value);
        if new.enabled() {
            match inner.hypercall_overlay.as_mut() {
                Some(overlay) => {
                    if overlay.move_to(new.gpa()) {
                        info!(&self.log, "moved hypercall page";
                              "guest_pfn" => new.gpfn());

                        WrmsrOutcome::Handled
                    } else {
                        WrmsrOutcome::GpException
                    }
                }
                None => {
                    let overlay = OverlayPage::create(
                        self.acc_mem.child(None),
                        new.gpa(),
                        &hypercall_page_contents(),
                    );

                    if overlay.is_some() {
                        info!(&self.log, "enabled hypercall page";
                              "guest_pfn" => new.gpfn());

                        inner.hypercall_overlay = overlay;
                        WrmsrOutcome::Handled
                    } else {
                        WrmsrOutcome::GpException
                    }
                }
            }
        } else {
            inner.hypercall_overlay.take();
            WrmsrOutcome::Handled
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
            MSR_GUEST_OS_ID => RdmsrOutcome::Handled(
                self.inner.lock().unwrap().msr_guest_os_id_value,
            ),
            MSR_HYPERCALL => RdmsrOutcome::Handled(
                self.inner.lock().unwrap().msr_hypercall_value.0,
            ),
            MSR_VP_INDEX => {
                let id: u32 = vcpu.into();
                RdmsrOutcome::Handled(id as u64)
            }
            _ => RdmsrOutcome::NotHandled,
        }
    }

    fn wrmsr(&self, _vcpu: VcpuId, msr: MsrId, value: u64) -> WrmsrOutcome {
        match msr.0 {
            MSR_GUEST_OS_ID => self.handle_wrmsr_guest_os_id(value),
            MSR_HYPERCALL => self.handle_wrmsr_hypercall(value),
            MSR_VP_INDEX => WrmsrOutcome::GpException,
            _ => WrmsrOutcome::NotHandled,
        }
    }

    fn attach(&self, mem_acc: &MemAccessor) {
        mem_acc.adopt(&self.acc_mem, Some(TYPE_NAME.to_owned()));
    }
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
