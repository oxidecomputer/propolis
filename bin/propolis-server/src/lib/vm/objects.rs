// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A collection of all of the components that make up a Propolis VM instance.

use std::{
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use propolis::{
    hw::{ps2::ctrl::PS2Ctrl, qemu::ramfb::RamFb, uart::LpcUart},
    vmm::VmmHdl,
    Machine,
};
use propolis_api_types::instance_spec::v0::InstanceSpecV0;
use slog::{error, info};
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::{serial::Serial, vcpu_tasks::VcpuTaskController};

use super::{
    state_driver::VmStartReason, BlockBackendMap, CrucibleBackendMap, DeviceMap,
};

/// A collection of components that make up a Propolis VM instance.
pub(crate) struct VmObjects {
    /// A reference to the VM state machine that created these objects. Used to
    /// complete rundown when the objects are dropped.
    parent: Arc<super::Vm>,

    /// Synchronizes access to the VM's objects.
    ///
    /// API-layer callers that want to enumerate a VM's devices or read its spec
    /// acquire this lock shared. The state driver acquires this lock exclusive
    /// to mutate the VM.
    inner: RwLock<VmObjectsLocked>,
}

/// A collection of objects that should eventually be wrapped in a lock and
/// stored in a `VmObjects` structure. See [`VmObjectsLocked`].
pub(super) struct InputVmObjects {
    pub instance_spec: InstanceSpecV0,
    pub vcpu_tasks: Box<dyn VcpuTaskController>,
    pub machine: Machine,
    pub devices: DeviceMap,
    pub block_backends: BlockBackendMap,
    pub crucible_backends: CrucibleBackendMap,
    pub com1: Arc<Serial<LpcUart>>,
    pub framebuffer: Option<Arc<RamFb>>,
    pub ps2ctrl: Arc<PS2Ctrl>,
}

/// The collection of objects and state that make up a Propolis instance.
pub(crate) struct VmObjectsLocked {
    /// The objects' associated logger.
    log: slog::Logger,

    /// The instance spec that describes this collection of objects.
    instance_spec: InstanceSpecV0,

    /// The set of tasks that run this VM's vCPUs.
    vcpu_tasks: Box<dyn VcpuTaskController>,

    /// The Propolis kernel VMM for this instance.
    machine: Machine,

    /// Maps from component names to the trait objects that implement lifecycle
    /// operations (e.g. pause and resume) for eligible components.
    devices: DeviceMap,

    /// Maps from component names to trait objects that implement the block
    /// storage backend trait.
    block_backends: BlockBackendMap,

    /// Maps from component names to Crucible backend objects.
    crucible_backends: CrucibleBackendMap,

    /// A handle to the serial console connection to the VM's first COM port.
    com1: Arc<Serial<LpcUart>>,

    /// A handle to the VM's framebuffer.
    framebuffer: Option<Arc<RamFb>>,

    /// A handle to the VM's PS/2 controller.
    ps2ctrl: Arc<PS2Ctrl>,
}

impl VmObjects {
    /// Creates a new VM object container.
    pub(super) fn new(
        log: slog::Logger,
        parent: Arc<super::Vm>,
        input: InputVmObjects,
    ) -> Self {
        let inner = VmObjectsLocked::new(&log, input);
        Self { parent, inner: tokio::sync::RwLock::new(inner) }
    }

    /// Yields a shared lock guard referring to the underlying object
    /// collection.
    pub(crate) async fn lock_shared(&self) -> VmObjectsShared {
        VmObjectsShared(self.inner.read().await)
    }

    /// Yields an exclusive lock guard referring to the underlying object
    /// collection.
    pub(crate) async fn lock_exclusive(&self) -> VmObjectsExclusive {
        VmObjectsExclusive(self.inner.write().await)
    }
}

impl VmObjectsLocked {
    /// Associates a collection of VM objects with a logger.
    fn new(log: &slog::Logger, input: InputVmObjects) -> Self {
        Self {
            log: log.clone(),
            instance_spec: input.instance_spec,
            vcpu_tasks: input.vcpu_tasks,
            machine: input.machine,
            devices: input.devices,
            block_backends: input.block_backends,
            crucible_backends: input.crucible_backends,
            com1: input.com1,
            framebuffer: input.framebuffer,
            ps2ctrl: input.ps2ctrl,
        }
    }

    /// Yields the VM's current instance spec.
    pub(crate) fn instance_spec(&self) -> &InstanceSpecV0 {
        &self.instance_spec
    }

    /// Yields a mutable reference to the VM's current instance spec.
    pub(crate) fn instance_spec_mut(&mut self) -> &mut InstanceSpecV0 {
        &mut self.instance_spec
    }

    /// Yields the VM's current Propolis VM aggregation.
    pub(crate) fn machine(&self) -> &Machine {
        &self.machine
    }

    /// Yields the VM's current kernel VMM handle.
    pub(crate) fn vmm_hdl(&self) -> &Arc<VmmHdl> {
        &self.machine.hdl
    }

    /// Yields an accessor to the VM's memory context, or None if guest memory
    /// is not currently accessible.
    pub(crate) fn access_mem(
        &self,
    ) -> Option<propolis::accessors::Guard<propolis::vmm::MemCtx>> {
        self.machine.acc_mem.access()
    }

    /// Obtains a handle to the lifecycle trait object for the component with
    /// the supplied `name`.
    pub(crate) fn device_by_name(
        &self,
        name: &str,
    ) -> Option<Arc<dyn propolis::common::Lifecycle>> {
        self.devices.get(name).cloned()
    }

    /// Yields the VM's current Crucible backend map.
    pub(crate) fn crucible_backends(&self) -> &CrucibleBackendMap {
        &self.crucible_backends
    }

    /// Yields a clonable reference to the serial console for this VM's first
    /// COM port.
    pub(crate) fn com1(&self) -> &Arc<Serial<LpcUart>> {
        &self.com1
    }

    /// Yields a clonable reference to this VM's framebuffer.
    pub(crate) fn framebuffer(&self) -> &Option<Arc<RamFb>> {
        &self.framebuffer
    }

    /// Yields a clonable reference to this VM's PS/2 controller.
    pub(crate) fn ps2ctrl(&self) -> &Arc<PS2Ctrl> {
        &self.ps2ctrl
    }

    /// Iterates over all of the lifecycle trait objects in this VM and calls
    /// `func` on each one.
    pub(crate) fn for_each_device(
        &self,
        mut func: impl FnMut(&str, &Arc<dyn propolis::common::Lifecycle>),
    ) {
        for (name, dev) in self.devices.iter() {
            func(name, dev);
        }
    }

    /// Iterates over all of the lifecycle objects in this VM and calls `func`
    /// on each one. If any invocation of `func` fails, this routine returns
    /// immediately and yields the relevant error.
    pub(crate) fn for_each_device_fallible<E>(
        &self,
        mut func: impl FnMut(
            &str,
            &Arc<dyn propolis::common::Lifecycle>,
        ) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        for (name, dev) in self.devices.iter() {
            func(name, dev)?;
        }

        Ok(())
    }

    /// Pauses the VM at the kernel VMM level, ensuring that in-kernel-emulated
    /// devices and vCPUs are brought to a consistent state.
    ///
    /// When the VM is paused, attempts to run its vCPUs (via `VM_RUN` ioctl)
    /// will fail.  A corresponding `resume_vm()` call must be made prior to
    /// allowing vCPU tasks to run.
    pub(super) fn pause_kernel_vm(&self) {
        info!(self.log, "pausing kernel VMM resources");
        self.machine.hdl.pause().expect("VM_PAUSE should succeed");
    }

    /// Resumes the VM at the kernel VMM level.
    pub(super) fn resume_kernel_vm(&self) {
        info!(self.log, "resuming kernel VMM resources");
        self.machine.hdl.resume().expect("VM_RESUME should succeed");
    }

    /// Reinitializes the VM by resetting all of its devices and its kernel VMM.
    pub(super) fn reset_devices_and_machine(&self) {
        self.for_each_device(|name, dev| {
            info!(self.log, "sending reset request to {}", name);
            dev.reset();
        });

        self.machine.reinitialize().unwrap();
    }

    /// Starts a VM's devices and allows all of its vCPU tasks to run.
    ///
    /// This function may be called either after initializing a new VM from
    /// scratch or after an inbound live migration. In the latter case, this
    /// routine assumes that the caller initialized and activated the VM's vCPUs
    /// prior to importing state from the migration source.
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

    /// Pauses this VM's devices and its kernel VMM.
    pub(crate) async fn pause(&mut self) {
        self.vcpu_tasks.pause_all();
        self.pause_devices().await;
        self.pause_kernel_vm();
    }

    /// Resumes this VM's devices and its kernel VMM.
    pub(crate) fn resume(&mut self) {
        self.resume_kernel_vm();
        self.resume_devices();
        self.vcpu_tasks.resume_all();
    }

    /// Stops the VM's vCPU tasks and devices.
    pub(super) async fn halt(&mut self) {
        self.vcpu_tasks.exit_all();
        self.halt_devices().await;
    }

    /// Resets the VM's kernel vCPU state.
    pub(super) fn reset_vcpus(&self) {
        self.vcpu_tasks.new_generation();
        self.reset_vcpu_state();
    }

    /// Hard-resets a VM by pausing, resetting, and resuming all its devices and
    /// vCPUs.
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

    /// Starts all of a VM's devices and allows its block backends to process
    /// requests from their devices.
    async fn start_devices(&self) -> anyhow::Result<()> {
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
                error!(self.log, "startup failed for {}: {:?}", name, e);
                return res;
            }
        }

        Ok(())
    }

    /// Pauses all of a VM's devices.
    async fn pause_devices(&self) {
        // Take care not to wedge the runtime with any device pause
        // implementations which might block.
        tokio::task::block_in_place(|| {
            self.for_each_device(|name, dev| {
                info!(self.log, "sending pause request to {}", name);
                dev.pause();
            });
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
                Pin::new(&mut mut_self.future)
                    .poll(cx)
                    .map(|_| mut_self.name.clone())
            }
        }

        info!(self.log, "waiting for devices to pause");
        let mut stream: FuturesUnordered<_> = self
            .devices
            .iter()
            .map(|(name, dev)| {
                info!(self.log, "got paused future from dev {}", name);
                NamedFuture { name: name.clone(), future: dev.paused() }
            })
            .collect();

        while let Some(name) = stream.next().await {
            info!(self.log, "dev {} completed pause", name);
        }

        info!(self.log, "all devices paused");
    }

    /// Resumes all of a VM's devices.
    fn resume_devices(&self) {
        self.for_each_device(|name, dev| {
            info!(self.log, "sending resume request to {}", name);
            dev.resume();
        })
    }

    /// Stops all of a VM's devices and detaches its block backends from their
    /// devices.
    async fn halt_devices(&self) {
        // Take care not to wedge the runtime with any device halt
        // implementations which might block.
        tokio::task::block_in_place(|| {
            self.for_each_device(|name, dev| {
                info!(self.log, "sending halt request to {}", name);
                dev.halt();
            });
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

    /// Resets a VM's kernel vCPU objects to their initial states.
    fn reset_vcpu_state(&self) {
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
        // It is always safe to complete rundown at this point because the state
        // driver ensures that if it creates VM objects, then it will not drop
        // them without first moving the VM to the Rundown state.
        let parent = self.parent.clone();
        tokio::spawn(async move {
            parent.complete_rundown().await;
        });
    }
}

/// A shared lock on the contents of a [`VmObjects`].
pub(crate) struct VmObjectsShared<'o>(RwLockReadGuard<'o, VmObjectsLocked>);

/// An exclusive lock on the contents of a [`VmObjects`].
pub(crate) struct VmObjectsExclusive<'o>(RwLockWriteGuard<'o, VmObjectsLocked>);

impl Deref for VmObjectsShared<'_> {
    type Target = VmObjectsLocked;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Deref for VmObjectsExclusive<'_> {
    type Target = VmObjectsLocked;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for VmObjectsExclusive<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
