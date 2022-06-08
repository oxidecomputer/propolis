//! Virtual CPU functionality.

use std::io::Result;
use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::dispatch::SyncCtx;
use crate::exits::{VmEntry, VmExit};
use crate::inventory::Entity;
use crate::migrate::{Migrate, MigrateStateError, Migrator};
use crate::vmm::VmmHdl;

use erased_serde::Serialize;

/// Alias for a function acting on a virtualized CPU within a dispatcher
/// callback.
pub type VcpuRunFunc = fn(&Vcpu, &mut SyncCtx);

/// A handle to a virtual CPU.
pub struct Vcpu {
    hdl: Arc<VmmHdl>,
    id: i32,
}

impl Vcpu {
    /// Creates a handle to a virtual CPU.
    pub(crate) fn new(hdl: Arc<VmmHdl>, id: i32) -> Arc<Self> {
        Arc::new(Self { hdl, id })
    }

    /// ID of the virtual CPU.
    pub fn cpuid(&self) -> i32 {
        self.id
    }

    pub fn is_bsp(&self) -> bool {
        self.id == 0
    }

    /// Sets the capabilities of the virtual CPU.
    pub fn set_default_capabs(&self) -> Result<()> {
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
    pub fn set_reg(&self, reg: bhyve_api::vm_reg_name, val: u64) -> Result<()> {
        let mut regcmd = bhyve_api::vm_register {
            cpuid: self.id,
            regnum: reg as i32,
            regval: val,
        };

        self.hdl.ioctl(bhyve_api::VM_SET_REGISTER, &mut regcmd)?;
        Ok(())
    }

    /// Gets the value of a register within the CPU.
    pub fn get_reg(&self, reg: bhyve_api::vm_reg_name) -> Result<u64> {
        let mut regcmd = bhyve_api::vm_register {
            cpuid: self.id,
            regnum: reg as i32,
            regval: 0,
        };

        self.hdl.ioctl(bhyve_api::VM_GET_REGISTER, &mut regcmd)?;
        Ok(regcmd.regval)
    }

    /// Set a segment register `reg` to a particular value `seg`.
    ///
    /// If `reg` is not a valid segment register, an error will
    /// be returned.
    pub fn set_segreg(
        &self,
        reg: bhyve_api::vm_reg_name,
        seg: &bhyve_api::seg_desc,
    ) -> Result<()> {
        let mut req = bhyve_api::vm_seg_desc {
            cpuid: self.id,
            regnum: reg as i32,
            desc: *seg,
        };

        self.hdl.ioctl(bhyve_api::VM_SET_SEGMENT_DESCRIPTOR, &mut req)?;
        Ok(())
    }

    /// Get the contents of segment register `reg`
    ///
    /// If `reg` is not a valid segment register, an error will
    /// be returned.
    pub fn get_segreg(
        &self,
        reg: bhyve_api::vm_reg_name,
    ) -> Result<bhyve_api::seg_desc> {
        let mut req = bhyve_api::vm_seg_desc {
            cpuid: self.id,
            regnum: reg as i32,
            desc: bhyve_api::seg_desc::default(),
        };

        self.hdl.ioctl(bhyve_api::VM_GET_SEGMENT_DESCRIPTOR, &mut req)?;
        Ok(req.desc)
    }

    /// Issues a command to reset all state for the virtual CPU (including registers and
    /// pending interrupts).
    pub fn reboot_state(&self) -> Result<()> {
        let mut vvr = bhyve_api::vm_vcpu_reset {
            vcpuid: self.id,
            kind: bhyve_api::vcpu_reset_kind::VRK_RESET as u32,
        };

        self.hdl.ioctl(bhyve_api::VM_RESET_CPU, &mut vvr)?;

        Ok(())
    }
    /// Activates the virtual CPU.
    ///
    /// Fails if the CPU has already been activated.
    pub fn activate(&self) -> Result<()> {
        let mut cpu = self.id;

        self.hdl.ioctl(bhyve_api::VM_ACTIVATE_CPU, &mut cpu)?;
        Ok(())
    }

    /// Set the state of a virtual CPU.
    pub fn set_run_state(&self, state: u32) -> Result<()> {
        let mut state = bhyve_api::vm_run_state {
            vcpuid: self.id,
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
    pub fn run(&self, entry: &VmEntry) -> Result<VmExit> {
        let mut exit: bhyve_api::vm_exit = Default::default();
        let mut entry = entry.to_raw(self.id, &mut exit);
        let _res = self.hdl.ioctl(bhyve_api::VM_RUN, &mut entry)?;
        Ok(VmExit::from(&exit))
    }

    /// Issues a "barrier" to the guest VM by polling a register.
    pub fn barrier(&self) -> Result<()> {
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

impl Entity for Vcpu {
    fn type_name(&self) -> &'static str {
        "bhyve-vcpu"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Custom(self)
    }
}
impl Migrate for Vcpu {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        Box::new(migrate::BhyveVcpuV1::read(self))
    }

    fn import(
        &self,
        _dev: &str,
        deserializer: &mut dyn erased_serde::Deserializer,
        _ctx: &DispCtx,
    ) -> std::result::Result<(), MigrateStateError> {
        // TODO: import deserialized state
        let _deserialized: migrate::BhyveVcpuV1 =
            erased_serde::deserialize(deserializer)?;
        Ok(())
    }
}

pub mod migrate {
    use std::io;

    use super::Vcpu;
    use crate::vmm;

    use bhyve_api::vm_reg_name;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Default, Deserialize, Serialize)]
    pub struct BhyveVcpuV1 {
        gp_regs: GpRegsV1,
        ctrl_regs: CtrlRegsV1,
        seg_regs: SegRegsV1,
        fpu_state: FpuStateV1,
        /// TODO: do not use the bhyve_api type
        lapic: bhyve_api::vdi_lapic_v1,
        ms_regs: Vec<MsrEntryV1>,
        // exception/interrupt state
    }
    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct GpRegsV1 {
        pub rax: u64,
        pub rcx: u64,
        pub rdx: u64,
        pub rbx: u64,
        pub rsp: u64,
        pub rbp: u64,
        pub rsi: u64,
        pub rdi: u64,
        pub r8: u64,
        pub r9: u64,
        pub r10: u64,
        pub r11: u64,
        pub r12: u64,
        pub r13: u64,
        pub r14: u64,
        pub r15: u64,

        pub rip: u64,
        pub rflags: u64,
    }
    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct CtrlRegsV1 {
        pub cr0: u64,
        pub cr2: u64,
        pub cr3: u64,
        pub cr4: u64,
        pub dr0: u64,
        pub dr1: u64,
        pub dr2: u64,
        pub dr3: u64,
        pub dr6: u64,
        pub dr7: u64,
    }

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct SegRegsV1 {
        pub cs: SegDescV1,
        pub ds: SegDescV1,
        pub es: SegDescV1,
        pub fs: SegDescV1,
        pub gs: SegDescV1,
        pub ss: SegDescV1,
        pub gdtr: SegDescV1,
        pub idtr: SegDescV1,
        pub ldtr: SegDescV1,
        pub tr: SegDescV1,
    }

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct SegDescV1 {
        pub base: u64,
        pub limit: u32,
        pub access: u32,
        pub selector: u16,
    }
    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct MsrEntryV1 {
        pub ident: u32,
        pub value: u64,
    }

    #[derive(Clone, Default, Deserialize, Serialize)]
    pub struct FpuStateV1 {
        pub blob: Vec<u8>,
    }

    // VM_REG_GUEST_EFER,
    // VM_REG_GUEST_PDPTE0,
    // VM_REG_GUEST_PDPTE1,
    // VM_REG_GUEST_PDPTE2,
    // VM_REG_GUEST_PDPTE3,
    // VM_REG_GUEST_INTR_SHADOW,
    impl GpRegsV1 {
        pub(super) fn read(vcpu: &Vcpu) -> io::Result<Self> {
            Ok(Self {
                rax: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RAX)?,
                rcx: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RCX)?,
                rdx: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RDX)?,
                rbx: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RBX)?,
                rsp: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RSP)?,
                rbp: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RBP)?,
                rsi: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RSI)?,
                rdi: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RDI)?,
                r8: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_R8)?,
                r9: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_R9)?,
                r10: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_R10)?,
                r11: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_R11)?,
                r12: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_R12)?,
                r13: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_R13)?,
                r14: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_R14)?,
                r15: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_R15)?,
                rip: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RIP)?,
                rflags: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RFLAGS)?,
            })
        }
    }
    impl CtrlRegsV1 {
        pub(super) fn read(vcpu: &Vcpu) -> io::Result<Self> {
            Ok(Self {
                cr0: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_CR0)?,
                cr2: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_CR2)?,
                cr3: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_CR3)?,
                cr4: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_CR4)?,
                dr0: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR0)?,
                dr1: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR1)?,
                dr2: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR2)?,
                dr3: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR3)?,
                dr6: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR6)?,
                dr7: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR7)?,
            })
        }
    }
    impl SegRegsV1 {
        pub(super) fn read(vcpu: &Vcpu) -> io::Result<Self> {
            let cs = SegDescV1::from_raw(
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_CS)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_CS)? as u16,
            );
            let ds = SegDescV1::from_raw(
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_DS)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DS)? as u16,
            );
            let es = SegDescV1::from_raw(
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_ES)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_ES)? as u16,
            );
            let fs = SegDescV1::from_raw(
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_FS)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_FS)? as u16,
            );
            let gs = SegDescV1::from_raw(
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_GS)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_GS)? as u16,
            );
            let ss = SegDescV1::from_raw(
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_SS)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_SS)? as u16,
            );
            let gdtr = SegDescV1::from_raw(
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_GDTR)?,
                0,
            );
            let idtr = SegDescV1::from_raw(
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_IDTR)?,
                0,
            );
            let ldtr = SegDescV1::from_raw(
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_LDTR)?,
                0,
            );
            let tr = SegDescV1::from_raw(
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_TR)?,
                0,
            );
            Ok(Self { cs, ds, es, fs, gs, ss, gdtr, idtr, ldtr, tr })
        }
    }
    impl SegDescV1 {
        fn from_raw(inp: bhyve_api::seg_desc, selector: u16) -> Self {
            Self {
                base: inp.base,
                limit: inp.limit,
                access: inp.access,
                selector,
            }
        }
    }
    impl From<bhyve_api::vdi_field_entry_v1> for MsrEntryV1 {
        fn from(raw: bhyve_api::vdi_field_entry_v1) -> Self {
            Self { ident: raw.vfe_ident, value: raw.vfe_value }
        }
    }
    impl FpuStateV1 {
        pub(super) fn read(vcpu: &Vcpu) -> io::Result<Self> {
            let mut fpu_area_desc = bhyve_api::vm_fpu_desc::default();

            vcpu.hdl.ioctl(bhyve_api::VM_DESC_FPU_AREA, &mut fpu_area_desc)?;
            let len = fpu_area_desc.vfd_req_size as usize;
            let mut fpu = Vec::with_capacity(len);
            fpu.resize_with(len, u8::default);

            let mut fpu_req = bhyve_api::vm_fpu_state {
                vcpuid: vcpu.cpuid(),
                buf: fpu.as_mut_ptr() as *mut libc::c_void,
                len: fpu_area_desc.vfd_req_size,
            };
            vcpu.hdl.ioctl(bhyve_api::VM_GET_FPU, &mut fpu_req)?;

            Ok(Self { blob: fpu })
        }
    }

    impl BhyveVcpuV1 {
        pub(super) fn read(vcpu: &Vcpu) -> Self {
            let hdl = &vcpu.hdl;
            let msrs: Vec<bhyve_api::vdi_field_entry_v1> =
                vmm::data::read_many(hdl, vcpu.cpuid(), bhyve_api::VDC_MSR, 1)
                    .unwrap();
            let lapic: bhyve_api::vdi_lapic_v1 =
                vmm::data::read(hdl, vcpu.cpuid(), bhyve_api::VDC_LAPIC, 1)
                    .unwrap();
            let res = Self {
                gp_regs: GpRegsV1::read(vcpu).unwrap(),
                ctrl_regs: CtrlRegsV1::read(vcpu).unwrap(),
                seg_regs: SegRegsV1::read(vcpu).unwrap(),
                fpu_state: FpuStateV1::read(vcpu).unwrap(),
                lapic,
                ms_regs: msrs.into_iter().map(MsrEntryV1::from).collect(),
            };
            res
        }
    }
}
