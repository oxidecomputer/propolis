// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Types and traits for handling guest-emitted events on the VM state driver.

use std::{collections::VecDeque, time::Duration};

/// An event raised by some component in the instance (e.g. a vCPU or the
/// chipset) that the state worker must handle.
///
/// The vCPU-sourced events carry a time element (duration since VM boot) as
/// emitted by the kernel vmm.  This is used to deduplicate events when all
/// vCPUs running in-kernel are kicked out for the suspend state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GuestEvent {
    /// Fired when the bhyve VM enters its halt state.
    VcpuSuspendHalt(Duration),
    /// Fired when the bhyve VM enters its reset state.
    VcpuSuspendReset(Duration),
    /// Fired when the bhyve VM resets due to a triple fault. The first element
    /// identifies the vCPU that sent this notification.
    VcpuSuspendTripleFault(i32, Duration),
    /// Chipset signaled halt condition
    ChipsetHalt,
    /// Chipset signaled reboot condition
    ChipsetReset,
}

#[derive(Debug, Default)]
pub(super) struct GuestEventQueue {
    queue: VecDeque<GuestEvent>,
}

/// A sink for events raised by a VM's vCPU tasks.
pub(crate) trait VcpuEventHandler: Send + Sync {
    fn suspend_halt_event(&self, when: Duration);
    fn suspend_reset_event(&self, when: Duration);
    fn suspend_triple_fault_event(&self, vcpu_id: i32, when: Duration);
    fn unhandled_vm_exit(
        &self,
        vcpu_id: i32,
        exit: propolis::exits::VmExitKind,
    );
    fn io_error_event(&self, vcpu_id: i32, error: std::io::Error);
}

/// A sink for events raised by a VM's chipset.
pub(crate) trait ChipsetEventHandler: Send + Sync {
    fn chipset_halt(&self);
    fn chipset_reset(&self);
}

impl GuestEventQueue {
    pub(super) fn enqueue(&mut self, event: GuestEvent) -> bool {
        if !self.queue.iter().any(|ev| *ev == event) {
            self.queue.push_back(event);
            true
        } else {
            false
        }
    }

    pub(super) fn pop_front(&mut self) -> Option<GuestEvent> {
        self.queue.pop_front()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}
