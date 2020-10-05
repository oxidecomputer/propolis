use std::io::Result;
use std::sync::Arc;

use crate::exits::{VmEntry, VmExit};
use crate::vm::VmmHdl;

use bhyve_api::{vm_reg_name, SEG_ACCESS_P, SEG_ACCESS_S};

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
        self.set_reg(vm_reg_name::VM_REG_GUEST_CR0, 0x6000_0010)?;
        self.set_reg(vm_reg_name::VM_REG_GUEST_RFLAGS, 0x0000_0002)?;

        let cs_desc = bhyve_api::seg_desc {
            base: 0xffff_0000,
            limit: 0xffff,
            // Present, R/W, Accessed
            access: SEG_ACCESS_P | SEG_ACCESS_S | 0x3,
        };
        self.set_segreg(vm_reg_name::VM_REG_GUEST_CS, &cs_desc)?;
        self.set_reg(vm_reg_name::VM_REG_GUEST_CS, 0xf000)?;

        let data_desc = bhyve_api::seg_desc {
            base: 0x0000_0000,
            limit: 0xffff,
            // Present, R/W, Accessed
            access: SEG_ACCESS_P | SEG_ACCESS_S | 0x3,
        };
        let data_segs = [
            vm_reg_name::VM_REG_GUEST_ES,
            vm_reg_name::VM_REG_GUEST_SS,
            vm_reg_name::VM_REG_GUEST_DS,
            vm_reg_name::VM_REG_GUEST_FS,
            vm_reg_name::VM_REG_GUEST_GS,
        ];
        for seg in &data_segs {
            self.set_segreg(*seg, &data_desc)?;
            self.set_reg(*seg, 0)?;
        }

        let gidtr_desc =
            bhyve_api::seg_desc { base: 0x0000_0000, limit: 0xffff, access: 0 };
        self.set_segreg(vm_reg_name::VM_REG_GUEST_GDTR, &gidtr_desc)?;
        self.set_segreg(vm_reg_name::VM_REG_GUEST_IDTR, &gidtr_desc)?;

        let ldtr_desc = bhyve_api::seg_desc {
            base: 0x0000_0000,
            limit: 0xffff,
            // LDT present
            access: SEG_ACCESS_P | 0b0010,
        };
        self.set_segreg(vm_reg_name::VM_REG_GUEST_LDTR, &ldtr_desc)?;

        let tr_desc = bhyve_api::seg_desc {
            base: 0x0000_0000,
            limit: 0xffff,
            // TSS32 busy
            access: SEG_ACCESS_P | 0b1011,
        };
        self.set_segreg(vm_reg_name::VM_REG_GUEST_TR, &tr_desc)?;

        Ok(())
    }
    pub fn activate(&mut self) -> Result<()> {
        let mut cpu = self.id;

        self.hdl.ioctl(bhyve_api::VM_ACTIVATE_CPU, &mut cpu)?;
        Ok(())
    }
    pub fn run(&mut self, entry: &VmEntry) -> Result<VmExit> {
        let mut exit: bhyve_api::vm_exit = Default::default();
        let mut entry = entry.to_raw(self.id, &mut exit);
        match self.hdl.ioctl(bhyve_api::VM_RUN, &mut entry) {
            Err(e) => {
                return Err(e);
            }
            Ok(_) => {}
        }
        Ok(VmExit::from(&exit))
    }
}
