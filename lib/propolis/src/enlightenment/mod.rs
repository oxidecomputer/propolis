// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Components that implement enlightenments: mechanisms that allow guest
//! software to cooperate directly with its host hypervisor.
//!
//! Although the high-level point of a virtual machine is to allow guest
//! software to behave as though it was running on its own physical hardware,
//! there are (as with any abstraction) places where a virtual machine is bound
//! to behave differently than a "real" computer. For example, accessing a piece
//! of chipset functionality (like a hardware timer) might be relatively
//! inexpensive on real hardware, but in a virtual machine it requires an
//! expensive VM exit. Similarly, guest OSes may generally assume that hardware
//! timer interrupts will be delivered and serviced very promptly--promptly
//! enough that they can be used as watchdogs to guarantee forward progress; not
//! so in a virtual machine, where the host may elect not to immediately run a
//! vCPU with a pending timer interrupt.
//!
//! To help smooth some of these problems over, many hypervisors implement a set
//! of "enlightenments" that allow the guest and the hypervisor to cooperate
//! directly with one another. The two sides can communicate in several ways:
//!
//! 1. The host can virtualize the CPUID instruction to allow the guest to
//!    identify the host's hypervisor type and its capabilities.
//! 2. The host can trap the RDMSR and WRMSR instructions to allow the guest
//!    to communicate via synthetic model-specific registers.
//! 3. The host and guest can agree to use pages of guest physical memory to
//!    share data.
//! 4. The guest can execute special instructions (VMCALL or VMMCALL) that exit
//!    to the host with a special exit type that indicates that the guest
//!    executed a "call hypervisor" instruction. Since the guest CPU's registers
//!    are saved on exit, the host can read them and interpret them as
//!    parameters to a virtual function call.
//!
//! Almost all hypervisors will at least identify themselves through CPUID (by
//! returning vendor information in leaf 0x4000_0000 ebx/ecx/edx); the use of
//! other CPUID leaves and other communication techniques is otherwise entirely
//! hypervisor vendor-dependent.
//!
//! This module defines traits that allow other Propolis components (notably
//! vCPUs) to interact with an enlightenment stack. This module's submodules
//! define various kinds of emulated hypervisor platforms and implement the
//! enlightenments they supply.

use std::sync::Arc;

use cpuid_utils::CpuidSet;

use crate::{
    common::{Lifecycle, VcpuId},
    msr::{MsrId, RdmsrOutcome, WrmsrOutcome},
};

pub mod bhyve;

/// A supertrait describing an enlightenment stack that can also receive
/// instance lifecycle notifications. Automatically implemented for any
/// component that implements both [`Enlightenment`] and [`Lifecycle`], which
/// includes all the enlightenment stacks in this module's submodules.
pub trait EnlightenmentDevice: Enlightenment + Lifecycle {
    /// Upcasts this object into an [`Enlightenment`].
    fn as_enlightenment(self: Arc<Self>) -> Arc<dyn Enlightenment>;

    /// Upcasts this object into a [`Lifecycle`] notification registrant.
    fn as_lifecycle(self: Arc<Self>) -> Arc<dyn Lifecycle>;
}

impl<T: Enlightenment + Lifecycle> EnlightenmentDevice for T {
    fn as_enlightenment(self: Arc<Self>) -> Arc<dyn Enlightenment> {
        self
    }

    fn as_lifecycle(self: Arc<Self>) -> Arc<dyn Lifecycle> {
        self
    }
}

/// Functionality provided by all enlightenment interfaces.
pub trait Enlightenment: Send + Sync {
    /// Adds this hypervisor interface's CPUID entries to `cpuid`.
    ///
    /// CPUID leaves from 0x4000_0000 to 0x4000_00FF are reserved for the
    /// hypervisor's use. On entry, the caller must ensure that `cpuid` does not
    /// contain any leaf entries in this range.
    fn add_cpuid(&self, cpuid: &mut CpuidSet) -> anyhow::Result<()>;

    /// Asks this enlightenment stack to attempt to handle an RDMSR instruction
    /// on the supplied `vcpu` targeting the supplied `msr`.
    fn rdmsr(&self, msr: MsrId, vcpu: VcpuId) -> RdmsrOutcome;

    /// Asks this enlightenment stack to attempt to handle a WRMSR instruction
    /// on the supplied `vcpu` that will write `value` to the supplied `msr`.
    fn wrmsr(&self, msr: MsrId, vcpu: VcpuId, value: u64) -> WrmsrOutcome;
}
