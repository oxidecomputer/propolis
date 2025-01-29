// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Components that expose a specific hypervisor interface to guest software.
//!
//! # Background
//!
//! Propolis and bhyve, like all VMMs and hypervisors, try to emulate a physical
//! computer faithfully enough that guest system software need not pay any heed
//! to the fact that it's running in a VM. But like all abstractions, the VM
//! abstraction is imperfect: operations that may be inexpensive on real
//! hardware may require a costly address space switch in a VM, CPU features may
//! work differently in a VM than they do on bare metal, and so forth.
//!
//! To help smooth over these sorts of difficulties, hypervisors may advertise
//! special integration features that allow guests to, e.g., avoid expensive VM
//! exits or account for the fact that their CPUs are not running on bare metal
//! but are scheduled by a hypervisor. These integration features are commonly
//! called "enlightenments." This module contains traits and types that allow
//! Propolis to expose various enlightenment interfaces to its guests.
//!
//! # Implementing a hypervisor interface
//!
//! Hypervisors advertise themselves to guests by intercepting and returning
//! special values when the guest executes the CPUID instruction:
//!
//! - Leaf 1 ecx bit 31 is conventionally used to indicate to system software
//!   that it is running in a VM.
//! - Intel and AMD guarantee that CPUID leaves from 0x4000_0000 to 0x4000_00FF
//!   are reserved for hypervisor use and will never be used to return
//!   identifying information on real hardware.
//! - By convention, leaf 0x4000_0000 identifies the maximum supported
//!   hypervisor leaf (in eax) and the hypervisor's vendor (in ebx/ecx/edx).
//!
//! Once a guest has detected its hypervisor's vendor, it may query additional
//! CPUID leaves in the hypervisor range to determine what enlightenments the
//! hypervisor has offered. (The meanings of the values returned by these other
//! leaves is vendor-specific.)
//!
//! Guests can communicate with hypervisors in a number of ways:
//!
//! - The hypervisor may advertise that it supports synthetic model-specific
//!   registers that the guest can access through the RDMSR or WRMSR
//!   instructions.
//! - The host CPU may provide a special instruction that exits the VM and
//!   exposes a well-known exit code to the host. Having detected that the
//!   guest executed this instruction, the host may read its saved register
//!   contents and interpret them as arguments to a guest-to-host "function
//!   call". These operations are commonly called "hypercalls".
//! - The guest and host may agree to share information on one or more pages of
//!   guest physical memory.
//!
//! Exactly which methods are used for which communications are
//! interface-specific.
//!
//! # This module
//!
//! This module provides the [`HypervisorInterface`] trait and submodules that
//! contain implementations of this trait. See the trait documentation and the
//! submodule documentation for more details.

use cpuid_utils::CpuidSet;

use crate::{
    common::{Lifecycle, VcpuId},
    msr::{MsrId, RdmsrOutcome, WrmsrOutcome},
};

pub mod bhyve;

/// The main trait a component must implement to serve a hypervisor interface to
/// a guest.
///
/// Enlightenment handlers may contain state that needs to be transferred over a
/// live migration (e.g., the values of synthetic hypervisor MSRs). Implementors
/// of this trait must therefore implement [`Lifecycle`] so that Propolis users
/// can issue callouts to them during VM state changes and live migrations.
pub trait HypervisorInterface: Lifecycle {
    /// Adds this hypervisor interface's CPUID entries to `cpuid`.
    ///
    /// CPUID leaves from 0x4000_0000 to 0x4000_00FF are reserved for the
    /// hypervisor's use. On entry, the caller must ensure that `cpuid` does not
    /// contain any leaf entries in this range.
    fn add_cpuid(&self, cpuid: &mut CpuidSet) -> anyhow::Result<()>;

    /// Asks this hypervisor interface to attempt to handle an RDMSR instruction
    /// on the supplied `vcpu` targeting the supplied `msr`.
    fn rdmsr(&self, msr: MsrId, vcpu: VcpuId) -> RdmsrOutcome;

    /// Asks this hypervisor interface to attempt to handle a WRMSR instruction
    /// on the supplied `vcpu` that will write `value` to the supplied `msr`.
    fn wrmsr(&self, msr: MsrId, vcpu: VcpuId, value: u64) -> WrmsrOutcome;
}
