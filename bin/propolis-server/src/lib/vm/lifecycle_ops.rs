// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use slog::{error, info};

impl super::VmObjects {
    /// Pause VM at the kernel VMM level, ensuring that in-kernel-emulated
    /// devices and vCPUs are brought to a consistent state.
    ///
    /// When the VM is paused, attempts to run its vCPUs (via `VM_RUN` ioctl)
    /// will fail.  A corresponding `resume_vm()` call must be made prior to
    /// allowing vCPU tasks to run.
    pub(super) fn pause_vm(&self) {
        info!(self.log, "pausing kernel VMM resources");
        self.machine.hdl.pause().expect("VM_PAUSE should succeed");
    }

    pub(super) fn resume_vm(&self) {
        info!(self.log, "resuming kernel VMM resources");
        self.machine.hdl.resume().expect("VM_RESUME should succeed");
    }

    pub(super) fn reset_devices_and_machine(&self) {
        self.for_each_device(|name, dev| {
            info!(self.log, "sending reset request to {}", name);
            dev.reset();
        });

        self.machine.reinitialize().unwrap();
    }

    pub(super) async fn start_devices(&self) -> anyhow::Result<()> {
        self.for_each_device_fallible(|name, dev| {
            info!(self.log, "sending startup complete to {}", name);
            let res = dev.start();
            if let Err(e) = &res {
                error!(self.log, "startup failed for {}: {:?}", name, e);
            }
            res
        })?;

        for (name, backend) in self.block_backends.iter() {
            info!(self.log, "starting block backend {}", name);
            let res = backend.start().await;
            if let Err(e) = &res {
                error!(self.log, "Startup failed for {}: {:?}", name, e);
                return res;
            }
        }

        Ok(())
    }

    pub(super) async fn pause_devices(&self) {
        self.for_each_device(|name, dev| {
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
        let mut stream: FuturesUnordered<_> = self
            .lifecycle_components
            .iter()
            .map(|(name, dev)| {
                info!(self.log, "got paused future from dev {}", name);
                NamedFuture { name: name.clone(), future: dev.paused() }
            })
            .collect();

        loop {
            match stream.next().await {
                Some(name) => {
                    info!(self.log, "dev {} completed pause", name);
                }

                None => {
                    info!(self.log, "all devices paused");
                    break;
                }
            }
        }
    }

    pub(super) fn resume_devices(&self) {
        self.for_each_device(|name, dev| {
            info!(self.log, "sending resume request to {}", name);
            dev.resume();
        })
    }

    pub(super) async fn halt_devices(&self) {
        self.for_each_device(|name, dev| {
            info!(self.log, "sending halt request to {}", name);
            dev.halt();
        });

        for (name, backend) in self.block_backends.iter() {
            info!(self.log, "stopping and detaching block backend {}", name);
            backend.stop().await;
            if let Err(err) = backend.detach() {
                error!(self.log, "error detaching block backend";
                       "name" => name,
                       "error" => ?err);
            }
        }
    }

    pub(super) fn reset_vcpu_state(&self) {
        for vcpu in self.machine.vcpus.iter() {
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
