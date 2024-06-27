// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Provides a type that collects all of the components that make up a Propolis
//! VM.

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use propolis::{
    hw::{ps2::ctrl::PS2Ctrl, qemu::ramfb::RamFb, uart::LpcUart},
    Machine,
};
use propolis_api_types::instance_spec::v0::InstanceSpecV0;
use slog::{error, info};
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::{serial::Serial, vcpu_tasks::VcpuTaskController};

use super::{
    state_driver::VmStartReason, BlockBackendMap, CrucibleBackendMap,
    LifecycleMap,
};

pub(crate) struct VmObjects {
    log: slog::Logger,
    parent: Arc<super::Vm>,
    inner: RwLock<VmObjectsLocked>,
}

pub(super) struct InputVmObjects {
    pub instance_spec: InstanceSpecV0,
    pub vcpu_tasks: Box<dyn VcpuTaskController>,
    pub machine: Machine,
    pub lifecycle_components: LifecycleMap,
    pub block_backends: BlockBackendMap,
    pub crucible_backends: CrucibleBackendMap,
    pub com1: Arc<Serial<LpcUart>>,
    pub framebuffer: Option<Arc<RamFb>>,
    pub ps2ctrl: Arc<PS2Ctrl>,
}

pub(crate) struct VmObjectsLocked {
    log: slog::Logger,
    instance_spec: InstanceSpecV0,
    vcpu_tasks: Box<dyn VcpuTaskController>,
    machine: Machine,
    lifecycle_components: LifecycleMap,
    block_backends: BlockBackendMap,
    crucible_backends: CrucibleBackendMap,
    com1: Arc<Serial<LpcUart>>,
    framebuffer: Option<Arc<RamFb>>,
    ps2ctrl: Arc<PS2Ctrl>,
}

impl VmObjects {
    pub(super) fn new(
        log: slog::Logger,
        parent: Arc<super::Vm>,
        input: InputVmObjects,
    ) -> Self {
        let inner = VmObjectsLocked::new(&log, input);
        Self { log, parent, inner: tokio::sync::RwLock::new(inner) }
    }

    pub(crate) fn log(&self) -> &slog::Logger {
        &self.log
    }

    pub(crate) async fn read(&self) -> RwLockReadGuard<VmObjectsLocked> {
        self.inner.read().await
    }

    pub(super) async fn write(&self) -> RwLockWriteGuard<VmObjectsLocked> {
        self.inner.write().await
    }
}

impl VmObjectsLocked {
    fn new(log: &slog::Logger, input: InputVmObjects) -> Self {
        Self {
            log: log.clone(),
            instance_spec: input.instance_spec,
            vcpu_tasks: input.vcpu_tasks,
            machine: input.machine,
            lifecycle_components: input.lifecycle_components,
            block_backends: input.block_backends,
            crucible_backends: input.crucible_backends,
            com1: input.com1,
            framebuffer: input.framebuffer,
            ps2ctrl: input.ps2ctrl,
        }
    }

    pub(crate) fn instance_spec(&self) -> &InstanceSpecV0 {
        &self.instance_spec
    }

    pub(crate) fn instance_spec_mut(&mut self) -> &mut InstanceSpecV0 {
        &mut self.instance_spec
    }

    pub(crate) fn machine(&self) -> &Machine {
        &self.machine
    }

    pub(crate) fn device_by_name(
        &self,
        name: &str,
    ) -> Option<Arc<dyn propolis::common::Lifecycle>> {
        self.lifecycle_components.get(name).cloned()
    }

    pub(crate) fn crucible_backends(&self) -> &CrucibleBackendMap {
        &self.crucible_backends
    }

    pub(crate) fn com1(&self) -> &Arc<Serial<LpcUart>> {
        &self.com1
    }

    pub(crate) fn framebuffer(&self) -> &Option<Arc<RamFb>> {
        &self.framebuffer
    }

    pub(crate) fn ps2ctrl(&self) -> &Arc<PS2Ctrl> {
        &self.ps2ctrl
    }

    pub(crate) fn for_each_device(
        &self,
        mut func: impl FnMut(&str, &Arc<dyn propolis::common::Lifecycle>),
    ) {
        for (name, dev) in self.lifecycle_components.iter() {
            func(name, dev);
        }
    }

    pub(crate) fn for_each_device_fallible<E>(
        &self,
        mut func: impl FnMut(
            &str,
            &Arc<dyn propolis::common::Lifecycle>,
        ) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        for (name, dev) in self.lifecycle_components.iter() {
            func(name, dev)?;
        }

        Ok(())
    }

    /// Pause VM at the kernel VMM level, ensuring that in-kernel-emulated
    /// devices and vCPUs are brought to a consistent state.
    ///
    /// When the VM is paused, attempts to run its vCPUs (via `VM_RUN` ioctl)
    /// will fail.  A corresponding `resume_vm()` call must be made prior to
    /// allowing vCPU tasks to run.
    pub(super) fn pause_kernel_vm(&self) {
        info!(self.log, "pausing kernel VMM resources");
        self.machine.hdl.pause().expect("VM_PAUSE should succeed");
    }

    pub(super) fn resume_kernel_vm(&self) {
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

    pub(super) async fn start(
        &mut self,
        reason: VmStartReason,
    ) -> anyhow::Result<()> {
        match reason {
            VmStartReason::ExplicitRequest => {
                self.reset_vcpus();
            }
            VmStartReason::MigratedIn => {
                self.resume_kernel_vm();
            }
        }

        let result = self.start_devices().await;
        if result.is_ok() {
            self.vcpu_tasks.resume_all();
        }

        result
    }

    pub(super) async fn pause(&mut self) {
        self.vcpu_tasks.pause_all();
        self.pause_devices().await;
        self.pause_kernel_vm();
    }

    pub(super) fn resume(&mut self) {
        self.resume_kernel_vm();
        self.resume_devices();
        self.vcpu_tasks.resume_all();
    }

    pub(super) async fn halt(&mut self) {
        self.vcpu_tasks.exit_all();
        self.halt_devices().await;
    }

    pub(super) fn reset_vcpus(&self) {
        self.vcpu_tasks.new_generation();
        self.reset_vcpu_state();
    }

    pub(super) async fn reboot(&mut self) {
        // Reboot is implemented as a pause -> reset -> resume transition.
        //
        // First, pause the vCPUs and all devices so no partially-completed
        // work is present.
        self.vcpu_tasks.pause_all();
        self.pause_devices().await;

        // Reset all entities and the VM's bhyve state, then reset the
        // vCPUs. The vCPU reset must come after the bhyve reset.
        self.reset_devices_and_machine();
        self.reset_vcpus();

        // Resume devices so they're ready to do more work, then resume
        // vCPUs.
        self.resume_devices();
        self.vcpu_tasks.resume_all();
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

impl Drop for VmObjects {
    fn drop(&mut self) {
        // Signal to these objects' owning VM that rundown has completed and a
        // new VM can be created.
        //
        // It is always safe to complete rundown at this point because an
        // `ActiveVm` always holds a reference to its `VmObjects`, and the
        // parent VM doesn't drop its `ActiveVm` until rundown begins.
        let parent = self.parent.clone();
        tokio::spawn(async move {
            parent.complete_rundown().await;
        });
    }
}
