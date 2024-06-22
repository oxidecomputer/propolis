// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use slog::{error, info};

/// Commands that the VM state driver can invoke on its active VM to pause,
/// resume, and reset the devices under its care.
///
/// These functions are abstracted into a trait to allow them to be mocked out
/// while testing the rest of the state driver.
#[cfg_attr(test, mockall::automock)]
pub(super) trait VmLifecycle: Send + Sync {
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
    fn start_devices(&self) -> BoxFuture<'_, anyhow::Result<()>>;

    /// Sends each device a pause request. Returns a future that can be awaited
    /// to wait for all pause requests to complete.
    fn pause_devices(&self) -> BoxFuture<'_, ()>;

    /// Sends each device a resume request.
    fn resume_devices(&self);

    /// Sends each device (and backend) a halt request.
    fn halt_devices(&self);

    /// Resets the state of each vCPU in the instance to its on-reboot state.
    fn reset_vcpu_state(&self);
}

impl VmLifecycle for super::ActiveVm {
    fn pause_vm(&self) {
        info!(self.log, "pausing kernel VMM resources");
        self.objects().machine().hdl.pause().expect("VM_PAUSE should succeed");
    }

    fn resume_vm(&self) {
        info!(self.log, "resuming kernel VMM resources");
        self.objects()
            .machine()
            .hdl
            .resume()
            .expect("VM_RESUME should succeed");
    }

    fn reset_devices_and_machine(&self) {
        self.for_each_device(|name, dev| {
            info!(self.log, "sending reset request to {}", name);
            dev.reset();
        });

        self.objects().machine().reinitialize().unwrap();
    }

    fn start_devices(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async {
            self.objects().for_each_device_fallible(|name, dev| {
                info!(self.log, "sending startup complete to {}", name);
                let res = dev.start();
                if let Err(e) = &res {
                    error!(self.log, "startup failed for {}: {:?}", name, e);
                }
                res
            })?;

            for (name, backend) in self.objects.block_backends.iter() {
                info!(self.log, "starting block backend {}", name);
                let res = backend.start().await;
                if let Err(e) = &res {
                    error!(self.log, "Startup failed for {}: {:?}", name, e);
                    return res;
                }
            }
            Ok(())
        })
    }

    fn pause_devices(&self) -> BoxFuture<'_, ()> {
        let objects = self.objects();
        objects.for_each_device(|name, dev| {
            info!(self.log, "sending pause request to {}", name);
            dev.pause();
        });

        struct NamedFuture {
            name: String,
            future: BoxFuture<'static, ()>,
        }

        impl std::future::Future for NamedFuture {
            type Output = String;

            fn poll(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Self::Output> {
                let mut_self = self.get_mut();
                match Pin::new(&mut mut_self.future).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(()) => Poll::Ready(mut_self.name.clone()),
                }
            }
        }

        info!(self.log, "waiting for devices to pause");
        let mut stream: FuturesUnordered<_> = objects
            .lifecycle_components
            .iter()
            .map(|(name, dev)| {
                info!(self.log, "got paused future from dev {}", name);
                NamedFuture { name: name.clone(), future: dev.paused() }
            })
            .collect();

        let log_fut = self.log.clone();
        Box::pin(async move {
            loop {
                match stream.next().await {
                    Some(name) => {
                        info!(log_fut, "dev {} completed pause", name);
                    }

                    None => {
                        info!(log_fut, "all devices paused");
                        break;
                    }
                }
            }
        })
    }

    fn resume_devices(&self) {
        self.objects().for_each_device(|name, dev| {
            info!(self.log, "sending resume request to {}", name);
            dev.resume();
        })
    }

    fn halt_devices(&self) {
        let objects = self.objects();
        objects.for_each_device(|name, dev| {
            info!(self.log, "sending halt request to {}", name);
            dev.halt();
        });

        for (name, backend) in objects.block_backends.iter() {
            info!(self.log, "stopping and detaching block backend {}", name);
            backend.stop();
            if let Err(err) = backend.detach() {
                error!(self.log, "error detaching block backend";
                       "name" => name,
                       "error" => ?err);
            }
        }
    }

    fn reset_vcpu_state(&self) {
        for vcpu in self.objects().machine().vcpus.iter() {
            info!(self.log, "resetting vCPU {}", vcpu.id);
            vcpu.activate().unwrap();
            vcpu.reboot_state().unwrap();
            if vcpu.is_bsp() {
                info!(self.log, "Resetting BSP vCPU {}", vcpu.id);
                vcpu.set_run_state(propolis::bhyve_api::VRS_RUN, None).unwrap();
                vcpu.set_reg(
                    propolis::bhyve_api::vm_reg_name::VM_REG_GUEST_RIP,
                    0xfff0,
                )
                .unwrap();
            }
        }
    }
}
