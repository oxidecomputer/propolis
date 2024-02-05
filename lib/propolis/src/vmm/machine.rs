// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Representation of a virtual machine's hardware.

use std::io::{Error, ErrorKind, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::accessors::*;
use crate::mmio::MmioBus;
use crate::pio::PioBus;
use crate::vcpu::{Vcpu, MAXCPU};
use crate::vmm::{create_vm, CreateOpts, PhysMap, VmmHdl};

/// Arbitrary limit for the top of the physical memory map.
///
/// For now it corresponds to the top address described by the DSDT in the
/// static tables shipped in the "blessed" OVMF ROM shipping with propolis.
///
/// When MMIO and the physmap in general is made more robust, this should be
/// eliminated completely.
pub const MAX_PHYSMEM: usize = 0x100_0000_0000;

/// The aggregate representation of a virtual machine.
pub struct Machine {
    pub hdl: Arc<VmmHdl>,
    pub vcpus: Vec<Arc<Vcpu>>,

    pub map_physmem: PhysMap,
    pub bus_mmio: Arc<MmioBus>,
    pub bus_pio: Arc<PioBus>,

    pub acc_mem: MemAccessor,
    pub acc_msi: MsiAccessor,

    // Track if machine has been destroyed prior to drop
    destroyed: AtomicBool,
}

impl Machine {
    pub fn reinitialize(&self) -> Result<()> {
        self.hdl.reinit(true)?;
        self.map_physmem.post_reinit()?;
        Ok(())
    }

    /// (Re)Initialize vCPUs per x86 spec
    pub fn vcpu_x86_setup(&self) -> Result<()> {
        for vcpu in self.vcpus.iter() {
            vcpu.activate()?;
            vcpu.reboot_state()?;
            if vcpu.is_bsp() {
                vcpu.set_run_state(bhyve_api::VRS_RUN, None)?;
                vcpu.set_reg(bhyve_api::vm_reg_name::VM_REG_GUEST_RIP, 0xfff0)?;
            }
        }
        Ok(())
    }

    /// Destroy the `Machine` and its associated resources.  Returns the
    /// underlying [VmmHdl](crate::vmm::VmmHdl) for the caller to do further
    /// cleanup, such as destroy the kernel VMM instance.
    pub fn destroy(mut self) -> Arc<VmmHdl> {
        self.do_destroy().expect("machine not already destroyed")
    }

    fn do_destroy(&mut self) -> Option<Arc<VmmHdl>> {
        if !self.destroyed.swap(true, Ordering::Relaxed) {
            // Poison the accessor roots so they may not be used further.
            self.acc_mem.poison().expect("memory accessor not poisoned");
            self.acc_msi.poison().expect("MSI accessor not poisoned");

            // Clear out registrations in the PIO/MMIO buses to reduce the
            // chances that they perpetuate a cyclic reference.
            self.bus_pio.clear();
            self.bus_mmio.clear();

            // Clear all of the entries from the physmem map so their associated
            // mappings in the process address space are munmapped.
            // TODO: more verification
            self.map_physmem.destroy();

            Some(self.hdl.clone())
        } else {
            None
        }
    }

    pub fn inject_nmi(&self) -> Result<()> {
        // When the Machine is created, we're guaranteed at least one vcpu, so
        // just send the NMI to the first one.
        self.vcpus[0].inject_nmi()
    }
}
impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.do_destroy();
    }
}

#[cfg(test)]
impl Machine {
    pub(crate) fn new_test() -> Result<Self> {
        // Create a test handle with 2M tempfile to use as our VM "memory"
        let hdl = Arc::new(VmmHdl::new_test(2 * 1024 * 1024)?);

        let mut map = PhysMap::new(MAX_PHYSMEM, hdl.clone());
        // TODO: meaningfully populate these
        map.add_test_rom("test-rom".to_string(), 0, 1024 * 1024)?;
        map.add_test_mem("test-ram".to_string(), 1024 * 1024, 1024 * 1024)?;

        let bus_mmio = Arc::new(MmioBus::new(MAX_PHYSMEM));
        let bus_pio = Arc::new(PioBus::new());

        let vcpus =
            vec![Vcpu::new(hdl.clone(), 0, bus_mmio.clone(), bus_pio.clone())];

        let acc_mem = MemAccessor::new(map.memctx());
        let acc_msi = MsiAccessor::new(hdl.clone());

        Ok(Machine {
            hdl: hdl.clone(),
            vcpus,

            map_physmem: map,

            acc_mem,
            acc_msi,

            bus_mmio,
            bus_pio,

            destroyed: AtomicBool::new(false),
        })
    }
}

/// Builder object used to initialize a [`Machine`].
///
/// # Example
///
/// ```no_run
/// use propolis::vmm::{Builder, CreateOpts};
///
/// let opts = CreateOpts {
///     // Override any desired VM creation options
///     ..Default::default()
/// };
/// let builder = Builder::new("my-machine", opts).unwrap()
///     .max_cpus(4).unwrap()
///     .add_mem_region(0, 0xc000_0000, "lowmem").unwrap()
///     .add_mem_region(0x1_0000_0000, 0xc000_0000, "highmem").unwrap()
///     .add_rom_region(0xffe0_0000, 0x20_0000, "bootrom").unwrap()
///     .add_mmio_region(0xc0000000, 0x20000000, "dev32").unwrap();
/// let machine = builder.finalize().unwrap();
/// ```
pub struct Builder {
    inner_hdl: Option<Arc<VmmHdl>>,
    physmap: Option<PhysMap>,
    max_cpu: u8,
}
impl Builder {
    /// Constructs a new builder object which may be used
    /// to produce a VM.
    ///
    /// In the construction of this object, the builder
    /// attempts to access the vmm controller at "/dev/vmmctl",
    /// and issues commands to begin construction of the VM.
    ///
    /// # Arguments
    /// - `name`: The name for the new instance.
    /// - `force`: If true, deletes the VM if it already exists.
    pub fn new(name: &str, opts: CreateOpts) -> Result<Self> {
        let hdl = Arc::new(create_vm(name, opts)?);
        let physmap = Some(PhysMap::new(MAX_PHYSMEM, hdl.clone()));
        Ok(Self { inner_hdl: Some(hdl), max_cpu: 1, physmap })
    }

    /// Creates and maps a memory segment in the guest's address space,
    /// identified as system memory.
    pub fn add_mem_region(
        mut self,
        start: usize,
        len: usize,
        name: &str,
    ) -> Result<Self> {
        self.physmap.as_mut().unwrap().add_mem(name.to_string(), start, len)?;
        Ok(self)
    }

    /// Creates and maps a memory segment in the guest's address space,
    /// identified as ROM.
    pub fn add_rom_region(
        mut self,
        start: usize,
        len: usize,
        name: &str,
    ) -> Result<Self> {
        self.physmap.as_mut().unwrap().add_rom(name.to_string(), start, len)?;
        Ok(self)
    }
    /// Registers a region of memory for MMIO.
    pub fn add_mmio_region(
        mut self,
        start: usize,
        len: usize,
        name: &str,
    ) -> Result<Self> {
        self.physmap.as_mut().unwrap().add_mmio_reservation(
            name.to_string(),
            start,
            len,
        )?;
        Ok(self)
    }
    /// Sets the maximum number of CPUs for the machine.
    pub fn max_cpus(mut self, max: u8) -> Result<Self> {
        if max == 0 || max > MAXCPU as u8 {
            Err(Error::new(ErrorKind::InvalidInput, "maxcpu out of range"))
        } else {
            self.max_cpu = max;
            Ok(self)
        }
    }

    /// Consumes `self` and creates a new [`Machine`] based
    /// on the provided memory regions.
    pub fn finalize(mut self) -> Result<Machine> {
        let hdl = self.inner_hdl.take().unwrap();
        let mut map = self.physmap.take().unwrap();

        let bus_mmio = Arc::new(MmioBus::new(MAX_PHYSMEM));
        let bus_pio = Arc::new(PioBus::new());

        let acc_mem = MemAccessor::new(map.memctx());
        let acc_msi = MsiAccessor::new(hdl.clone());

        let vcpus = (0..self.max_cpu)
            .map(|id| {
                Vcpu::new(
                    hdl.clone(),
                    id as i32,
                    bus_mmio.clone(),
                    bus_pio.clone(),
                )
            })
            .collect();

        let machine = Machine {
            hdl: hdl.clone(),
            vcpus,

            map_physmem: map,

            acc_mem,
            acc_msi,

            bus_mmio,
            bus_pio,

            destroyed: AtomicBool::new(false),
        };
        Ok(machine)
    }
}
impl Drop for Builder {
    fn drop(&mut self) {
        if let Some(hdl) = &self.inner_hdl {
            // Do not allow the vmm device to persist
            hdl.destroy().unwrap();
        }
    }
}
