// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// Commands that the VM state driver can invoke on its active VM to pause,
/// resume, and reset the devices under its care.
///
/// These functions are abstracted into a trait to allow them to be mocked out
/// while testing the rest of the state driver.
#[cfg_attr(test, mockall::automock)]
pub(super) trait VmLifecycle {
    /// Pause VM at the kernel VMM level, ensuring that in-kernel-emulated
    /// devices and vCPUs are brought to a consistent state.
    ///
    /// When the VM is paused, attempts to run its vCPUs (via `VM_RUN` ioctl)
    /// will fail.  A corresponding `resume_vm()` call must be made prior to
    /// allowing vCPU tasks to run.
    fn pause_vm(&self);

    /// Resume a previously-paused VM at the kernel VMM level.  This will resume
    /// any timers driving in-kernel-emulated devices, and allow the vCPU to run
    /// again.
    fn resume_vm(&self);

    /// Sends a reset request to each device in the instance, then sends a
    /// reset command to the instance's bhyve VM.
    fn reset_devices_and_machine(&self);

    /// Sends each device (and backend) a start request.
    fn start_devices(&self) -> anyhow::Result<()>;

    /// Sends each device a pause request, then waits for all these requests to
    /// complete.
    fn pause_devices(&self);

    /// Sends each device a resume request.
    fn resume_devices(&self);

    /// Sends each device (and backend) a halt request.
    fn halt_devices(&self);

    /// Resets the state of each vCPU in the instance to its on-reboot state.
    fn reset_vcpu_state(&self);
}

impl VmLifecycle for super::ActiveVm {
    fn pause_vm(&self) {
        self.objects.machine.hdl.pause().expect("VM_PAUSE should succeed");
    }

    fn resume_vm(&self) {
        self.objects.machine.hdl.resume().expect("VM_RESUME should succeed");
    }

    fn reset_devices_and_machine(&self) {
        self.objects.for_each_device(|name, dev| {
            dev.reset();
        });

        self.objects.machine.reinitialize().unwrap();
    }

    fn start_devices(&self) -> anyhow::Result<()> {
        todo!()
    }

    fn pause_devices(&self) {
        todo!()
    }

    fn resume_devices(&self) {
        todo!()
    }

    fn halt_devices(&self) {
        todo!()
    }

    fn reset_vcpu_state(&self) {
        todo!()
    }
}
