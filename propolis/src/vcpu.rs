use std::io::Result;
use std::sync::Arc;

use crate::exits::{VmEntry, VmExit};
use crate::vmm::VmmHdl;

pub struct VcpuHdl {
    hdl: Arc<VmmHdl>,
    id: i32,
}

impl VcpuHdl {
    pub fn from_vmhdl(hdl: Arc<VmmHdl>, id: i32) -> Self {
        Self { hdl, id }
    }

    pub fn cpuid(&self) -> i32 {
        self.id
    }

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
    pub fn reboot_state(&mut self) -> Result<()> {
        let mut vvr = bhyve_api::vm_vcpu_reset {
            cpuid: self.id,
            kind: bhyve_api::vcpu_reset_kind::VRK_RESET as u32,
        };

        self.hdl.ioctl(bhyve_api::VM_RESET_CPU, &mut vvr)?;

        Ok(())
    }
    pub fn activate(&mut self) -> Result<()> {
        let mut cpu = self.id;

        self.hdl.ioctl(bhyve_api::VM_ACTIVATE_CPU, &mut cpu)?;
        Ok(())
    }
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
    pub fn run(&mut self, entry: &VmEntry) -> Result<VmExit> {
        let mut exit: bhyve_api::vm_exit = Default::default();
        let mut entry = entry.to_raw(self.id, &mut exit);
        let _res = self.hdl.ioctl(bhyve_api::VM_RUN, &mut entry)?;
        Ok(VmExit::from(&exit))
    }
}
