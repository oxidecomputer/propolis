//! Virtual CPU functionality.

use std::io::Result;
use std::sync::Arc;

use crate::dispatch::SyncCtx;
use crate::exits::{VmEntry, VmExit};
use crate::vmm::VmmHdl;

/// A handle to a virtual CPU.
pub struct VcpuHdl {
    hdl: Arc<VmmHdl>,
    id: i32,
}

impl VcpuHdl {
    /// Creates a handle to a virtual CPU.
    pub(crate) fn new(hdl: Arc<VmmHdl>, id: i32) -> Self {
        Self { hdl, id }
    }

    /// ID of the virtual CPU.
    pub fn cpuid(&self) -> i32 {
        self.id
    }

    pub fn is_bsp(&self) -> bool {
        self.id == 0
    }

    /// Sets the capabilities of the virtual CPU.
    pub fn set_default_capabs(&mut self) -> Result<()> {
        // Enable exit-on-HLT so the host CPU does not spin in VM context when
        // the guest enters a HLT instruction.
        let mut cap = bhyve_api::vm_capability {
            cpuid: self.id,
            captype: bhyve_api::vm_cap_type::VM_CAP_HALT_EXIT as i32,
            capval: 1,
            allcpus: 0,
        };
        self.hdl.ioctl(bhyve_api::VM_SET_CAPABILITY, &mut cap)
    }

    /// Sets the value of a register within the CPU.
    pub fn set_reg(
        &mut self,
        reg: bhyve_api::vm_reg_name,
        val: u64,
    ) -> Result<()> {
        let mut regcmd = bhyve_api::vm_register {
            cpuid: self.id,
            regnum: reg as i32,
            regval: val,
        };

        self.hdl.ioctl(bhyve_api::VM_SET_REGISTER, &mut regcmd)?;
        Ok(())
    }

    /// Set a segment register `reg` to a particular value `seg`.
    ///
    /// If `reg` is not a valid segment register, an error will
    /// be returned.
    pub fn set_segreg(
        &mut self,
        reg: bhyve_api::vm_reg_name,
        seg: &bhyve_api::seg_desc,
    ) -> Result<()> {
        let mut desc = bhyve_api::vm_seg_desc {
            cpuid: self.id,
            regnum: reg as i32,
            desc: *seg,
        };

        self.hdl.ioctl(bhyve_api::VM_SET_SEGMENT_DESCRIPTOR, &mut desc)?;
        Ok(())
    }

    /// Issues a command to reset all state for the virtual CPU (including registers and
    /// pending interrupts).
    pub fn reboot_state(&mut self) -> Result<()> {
        let mut vvr = bhyve_api::vm_vcpu_reset {
            cpuid: self.id,
            kind: bhyve_api::vcpu_reset_kind::VRK_RESET as u32,
        };

        self.hdl.ioctl(bhyve_api::VM_RESET_CPU, &mut vvr)?;

        Ok(())
    }
    /// Activates the virtual CPU.
    ///
    /// Fails if the CPU has already been activated.
    pub fn activate(&mut self) -> Result<()> {
        let mut cpu = self.id;

        self.hdl.ioctl(bhyve_api::VM_ACTIVATE_CPU, &mut cpu)?;
        Ok(())
    }

    /// Set the state of a virtual CPU.
    pub fn set_run_state(&mut self, state: u32) -> Result<()> {
        let mut state = bhyve_api::vm_run_state {
            cpuid: self.id,
            state,
            sipi_vector: 0,
            ..Default::default()
        };
        self.hdl.ioctl(bhyve_api::VM_SET_RUN_STATE, &mut state)?;
        Ok(())
    }

    /// Executes the guest by running the virtual CPU.
    ///
    /// Blocks the calling thread until the vCPU returns execution,
    /// and returns the reason for exiting ([`VmExit`]).
    pub fn run(&mut self, entry: &VmEntry) -> Result<VmExit> {
        let mut exit: bhyve_api::vm_exit = Default::default();
        let mut entry = entry.to_raw(self.id, &mut exit);
        let _res = self.hdl.ioctl(bhyve_api::VM_RUN, &mut entry)?;
        Ok(VmExit::from(&exit))
    }

    /// Issues a "barrier" to the guest VM by polling a register.
    pub fn barrier(&mut self) -> Result<()> {
        // XXX: without an official interface for this, just force the vCPU out
        // of guest context (if it is there) by reading %rax.
        let mut regcmd = bhyve_api::vm_register {
            cpuid: self.id,
            regnum: bhyve_api::vm_reg_name::VM_REG_GUEST_RAX as i32,
            regval: 0,
        };
        self.hdl.ioctl(bhyve_api::VM_GET_REGISTER, &mut regcmd)?;
        Ok(())
    }
}

/// Alias for a function acting on a virtualized CPU within a dispatcher
/// callback.
pub type VcpuRunFunc = fn(VcpuHdl, &mut SyncCtx);
