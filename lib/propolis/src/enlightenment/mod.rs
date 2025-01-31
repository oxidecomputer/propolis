// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Components that implement _enlightenments_: mechanisms that allow guest
//! software to cooperate directly with its host hypervisor.
//!
//! # Background
//!
//! Although the high-level point of a virtual machine is to allow guest
//! software to behave as though it was running on its own physical hardware,
//! there are (as with any abstraction) places where a virtual machine is bound
//! to behave differently than a "real" computer. For example, accessing a piece
//! of chipset functionality (like a hardware timer) might be relatively
//! inexpensive on real hardware, but in a virtual machine it requires an
//! expensive VM exit. Similarly, guest OSes may generally assume that hardware
//! timer interrupts will be delivered and serviced very promptly---promptly
//! enough that they can be used as watchdogs to guarantee forward progress; not
//! so in a virtual machine, where the host may elect not to immediately run a
//! vCPU with a pending timer interrupt.
//!
//! To help smooth some of these problems over, many hypervisors implement a set
//! of "enlightenments" that allow the guest and the hypervisor to cooperate
//! directly with one another. This module distinguishes enlightenments from
//! other kinds of virtual devices by the interfaces the guest uses to
//! communicate with the host. For hypervisor enlightenments these include the
//! following:
//!
//! 1. CPUID: The enlightenment stack injects synthetic CPUID values in a
//!    well-known range of leaves (beginning with leaf 0x4000_0000) to advertise
//!    its capabilities to guests.
//! 2. Synthetic MSRs: The enlightenment stack intercepts RDMSR and WRMSR
//!    instructions targeting MSRs in a well-known range (beginning with MSR ID
//!    and interprets them according to its interface's definitions.
//! 3. Direct sharing of guest physical memory: The guest can use MSRs to offer
//!    to share its physical pages with the host, either to communicate directly
//!    or for the host to overlay with larger blocks of information the guest
//!    may wish to read.
//! 4. Special VM exits: Both Intel's VMX and AMD's SVM provide special opcodes
//!    (VMCALL and VMMCALL, respectively) that trigger a VM exit with a unique
//!    exit code. The hypervisor can detect exits with this code, read the
//!    guest's registers, and interpret them as parameters to a virtual function
//!    call.
//!
//! Enlightenment stacks generally do not use port I/O or memory-mapped I/O
//! to receive data from guests. This distinguishes them from other purely
//! virtual devices (like virtio devices or the pvpanic device) that do not
//! emulate any particular kind of physical hardware but nevertheless manifest
//! themselves to the guest as attachments to a virtual bus.
//!
//! # This module
//!
//! This module defines traits that allow other Propolis components (notably
//! vCPUs) to interact with an enlightenment stack. This module's submodules
//! define various kinds of emulated hypervisor platforms and implement the
//! enlightenments they supply.

use std::sync::Arc;

use cpuid_utils::CpuidSet;

use crate::{
    accessors::MemAccessor,
    common::{Lifecycle, VcpuId},
    msr::{MsrId, RdmsrOutcome, WrmsrOutcome},
};

pub mod bhyve;

/// Functionality provided by all enlightenment interfaces.
pub trait Enlightenment: Lifecycle + Send + Sync {
    fn as_lifecycle(self: Arc<Self>) -> Arc<dyn Lifecycle>
    where
        Self: Sized,
    {
        self
    }

    /// Attaches this enlightenment stack to a VM.
    ///
    /// Users of an enlightenment stack must guarantee that this function is
    /// called exactly once per instance of that stack.
    ///
    /// # Arguments
    ///
    /// - `mem_acc`: Supplies the root memory accessor for this stack's VM.
    ///   Stacks that wish to access guest memory should call
    ///   [`MemAccessor::new_orphan`] when they're created and then should call
    ///   [`MemAccessor::adopt`] from this function.
    fn attach(&self, mem_acc: &MemAccessor);

    /// Adds this hypervisor interface's CPUID entries to `cpuid`.
    ///
    /// CPUID leaves from 0x4000_0000 to 0x4000_00FF are reserved for the
    /// hypervisor's use. On entry, the caller must ensure that `cpuid` does not
    /// contain any leaf entries in this range.
    fn add_cpuid(&self, cpuid: &mut CpuidSet) -> anyhow::Result<()>;

    /// Asks this enlightenment stack to attempt to handle an RDMSR instruction
    /// on the supplied `vcpu` targeting the supplied `msr`.
    fn rdmsr(&self, vcpu: VcpuId, msr: MsrId) -> RdmsrOutcome;

    /// Asks this enlightenment stack to attempt to handle a WRMSR instruction
    /// on the supplied `vcpu` that will write `value` to the supplied `msr`.
    fn wrmsr(&self, vcpu: VcpuId, msr: MsrId, value: u64) -> WrmsrOutcome;
}
