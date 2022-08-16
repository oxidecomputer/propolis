//! Structures related VM instances management.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::inventory::Inventory;
use crate::vmm::Machine;

struct Inner {
    machine: Option<Machine>,
    inventory: Arc<Inventory>,
}

/// A single virtual machine.
pub struct Instance(Mutex<Inner>);
impl Instance {
    /// Creates a new virtual machine, absorbing `machine` generated from
    /// a `machine::Builder`.
    pub fn create(machine: Machine) -> Self {
        let inv = Arc::new(Inventory::new());
        // Register bhyve-internal devices
        machine.kernel_devs.register(&inv);
        for vcpu in machine.vcpus.iter() {
            inv.register_instance(vcpu, vcpu.cpuid()).unwrap();
        }

        Self(Mutex::new(Inner { machine: Some(machine), inventory: inv }))
    }

    pub fn lock(&self) -> InstanceGuard {
        InstanceGuard::new(self)
    }

    /// Return the [`Inventory`] describing the device tree of this Instance.
    pub fn inv(&self) -> Arc<Inventory> {
        let state = self.0.lock().unwrap();
        Arc::clone(&state.inventory)
    }

    pub fn destroy(self) -> Machine {
        let mut guard = self.0.lock().unwrap();
        // Clear the contets of the inventory and emit the underlying machine
        guard.inventory.clear();
        guard.machine.take().unwrap()
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        // Make sure the inventory is cleared, even if the consumer chose not to
        // call Instance::destroy().
        self.0.get_mut().unwrap().inventory.clear()
    }
}

pub struct InstanceGuard<'a> {
    inner: MutexGuard<'a, Inner>,
}
impl<'a> InstanceGuard<'a> {
    fn new(inst: &'a Instance) -> Self {
        let inner = inst.0.lock().unwrap();
        Self { inner }
    }
    pub fn machine(&self) -> &Machine {
        &self.inner.machine.as_ref().unwrap()
    }
    pub fn inventory(&self) -> &Inventory {
        &self.inner.inventory
    }
}

#[cfg(test)]
impl Instance {
    pub fn new_test() -> std::io::Result<Self> {
        Ok(Self::create(Machine::new_test()?))
    }
}
