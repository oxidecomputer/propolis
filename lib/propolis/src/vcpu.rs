// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Virtual CPU functionality.

use std::io::{Error, ErrorKind, Result};
use std::sync::Arc;

use crate::cpuid;
use crate::exits::*;
use crate::inventory::Entity;
use crate::migrate::*;
use crate::mmio::MmioBus;
use crate::pio::PioBus;
use crate::tasks;
use crate::vmm::VmmHdl;
use migrate::VcpuReadWrite;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn vm_entry(vcpuid: u32) {}
    fn vm_exit(vcpuid: u32, rip: u64, code: u32) {}
}

#[cfg(not(feature = "omicron-build"))]
pub const MAXCPU: usize = bhyve_api::VM_MAXCPU;

// Helios (stlouis) is built with an expanded limit of 64
#[cfg(feature = "omicron-build")]
pub const MAXCPU: usize = 64;

/// A handle to a virtual CPU.
pub struct Vcpu {
    hdl: Arc<VmmHdl>,
    pub id: i32,
    pub bus_mmio: Arc<MmioBus>,
    pub bus_pio: Arc<PioBus>,
}

impl Vcpu {
    /// Creates a handle to a virtual CPU.
    pub(crate) fn new(
        hdl: Arc<VmmHdl>,
        id: i32,
        bus_mmio: Arc<MmioBus>,
        bus_pio: Arc<PioBus>,
    ) -> Arc<Self> {
        Arc::new(Self { hdl, id, bus_mmio, bus_pio })
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
        unsafe { self.hdl.ioctl(bhyve_api::VM_SET_CAPABILITY, &mut cap) }
    }

    /// Sets the value of a register within the CPU.
    pub fn set_reg(&self, reg: bhyve_api::vm_reg_name, val: u64) -> Result<()> {
        let mut regcmd = bhyve_api::vm_register {
            cpuid: self.id,
            regnum: reg as i32,
            regval: val,
        };

        unsafe {
            self.hdl.ioctl(bhyve_api::VM_SET_REGISTER, &mut regcmd)?;
        }
        Ok(())
    }

    /// Gets the value of a register within the CPU.
    pub fn get_reg(&self, reg: bhyve_api::vm_reg_name) -> Result<u64> {
        let mut regcmd = bhyve_api::vm_register {
            cpuid: self.id,
            regnum: reg as i32,
            regval: 0,
        };

        unsafe {
            self.hdl.ioctl(bhyve_api::VM_GET_REGISTER, &mut regcmd)?;
        }
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

        unsafe {
            self.hdl.ioctl(bhyve_api::VM_SET_SEGMENT_DESCRIPTOR, &mut req)?;
        }
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

        unsafe {
            self.hdl.ioctl(bhyve_api::VM_GET_SEGMENT_DESCRIPTOR, &mut req)?;
        }
        Ok(req.desc)
    }

    /// Configure the (in-kernel) `cpuid` emulation state for this vCPU.
    ///
    /// If `values` contains no cpuid entries, then legacy emulation handling
    /// will be used.
    pub fn set_cpuid(&self, values: cpuid::Set) -> Result<()> {
        let mut config = bhyve_api::vm_vcpu_cpuid_config {
            vvcc_vcpuid: self.id,
            ..Default::default()
        };
        if values.is_empty() {
            config.vvcc_flags = bhyve_api::VCC_FLAG_LEGACY_HANDLING;
            unsafe {
                self.hdl.ioctl(bhyve_api::VM_SET_CPUID, &mut config)?;
            }
        } else {
            if values.vendor.is_intel() {
                config.vvcc_flags |= bhyve_api::VCC_FLAG_INTEL_FALLBACK;
            }
            let mut entries: Vec<bhyve_api::vcpu_cpuid_entry> = values.into();
            entries.sort_by(bhyve_api::vcpu_cpuid_entry::eval_sort);
            config.vvcc_nent = entries.len() as u32;
            config.vvcc_entries = entries.as_mut_ptr() as *mut libc::c_void;
            unsafe {
                self.hdl.ioctl(bhyve_api::VM_SET_CPUID, &mut config)?;
            }
        }

        Ok(())
    }

    /// Query the configured (in-kernel) `cpuid` emulation state for this vCPU.
    ///
    /// If legacy cpuid handling is configured, the resulting [Set](cpuid::Set)
    /// will contain no entries.
    pub fn get_cpuid(&self) -> Result<cpuid::Set> {
        let mut config = bhyve_api::vm_vcpu_cpuid_config {
            vvcc_vcpuid: self.id,
            vvcc_nent: 0,
            ..Default::default()
        };
        // Query the number of entries configured in-kernel
        //
        // We expect an error (E2BIG) when attempting a VM_GET_CPUID with a
        // vvcc_nent which falls below the number of entries stored in the
        // kernel.  When that occurs, vvcc_nent will be updated with that
        // existing count so we may allocate an array to receive it on a
        // subsquent ioctl.
        let count = match unsafe {
            self.hdl.ioctl(bhyve_api::VM_GET_CPUID, &mut config)
        } {
            Err(_) if config.vvcc_nent != 0 => Ok(config.vvcc_nent),
            Ok(_) => {
                assert_eq!(config.vvcc_nent, 0);
                Ok(0)
            }
            Err(e) => Err(e),
        }?;

        let mut entries = Vec::with_capacity(count as usize);
        entries.fill(bhyve_api::vcpu_cpuid_entry::default());
        config.vvcc_entries = entries.as_mut_ptr() as *mut libc::c_void;
        unsafe {
            self.hdl.ioctl(bhyve_api::VM_GET_CPUID, &mut config)?;
        }

        if config.vvcc_flags & bhyve_api::VCC_FLAG_LEGACY_HANDLING != 0 {
            // Since the legacy handling takes care of vendor-specific handling
            // (by nature of doing the cpuid queries against the host CPU) it
            // ignores the INTEL_FALLBACK flag.  We must determine the vendor
            // kind by querying it.
            let vendor = cpuid::VendorKind::try_from(cpuid::host_query(
                cpuid::Ident(0, None),
            ))
            .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?;

            return Ok(cpuid::Set::new(vendor));
        }
        let intel_fallback =
            config.vvcc_flags & bhyve_api::VCC_FLAG_INTEL_FALLBACK != 0;
        let mut set = cpuid::Set::new(match intel_fallback {
            true => cpuid::VendorKind::Intel,
            false => cpuid::VendorKind::Amd,
        });

        for entry in entries {
            let (ident, value) = cpuid::from_raw(entry);
            let conflict = set.insert(ident, value);
            if conflict.is_some() {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "conflicting entry at eax:{:x} ecx:{:x?})",
                        ident.0, ident.1
                    ),
                ));
            }
        }
        Ok(set)
    }

    /// Issues a command to reset all state for the virtual CPU (including registers and
    /// pending interrupts).
    pub fn reboot_state(&self) -> Result<()> {
        let mut vvr = bhyve_api::vm_vcpu_reset {
            vcpuid: self.id,
            kind: bhyve_api::vcpu_reset_kind::VRK_RESET as u32,
        };

        unsafe {
            self.hdl.ioctl(bhyve_api::VM_RESET_CPU, &mut vvr)?;
        }

        Ok(())
    }
    /// Activates the virtual CPU.
    ///
    /// Fails if the CPU has already been activated.
    pub fn activate(&self) -> Result<()> {
        let mut cpu = self.id;

        unsafe {
            self.hdl.ioctl(bhyve_api::VM_ACTIVATE_CPU, &mut cpu)?;
        }
        Ok(())
    }

    /// Set the state of a virtual CPU.
    pub fn set_run_state(
        &self,
        state: u32,
        sipi_vector: Option<u8>,
    ) -> Result<()> {
        let mut state = bhyve_api::vm_run_state {
            vcpuid: self.id,
            state,
            sipi_vector: sipi_vector.unwrap_or(0),
            ..Default::default()
        };
        unsafe {
            self.hdl.ioctl(bhyve_api::VM_SET_RUN_STATE, &mut state)?;
        }
        Ok(())
    }

    /// Get the state of the virtual CPU.
    pub fn get_run_state(&self) -> Result<bhyve_api::vm_run_state> {
        let mut state =
            bhyve_api::vm_run_state { vcpuid: self.id, ..Default::default() };
        unsafe {
            self.hdl.ioctl(bhyve_api::VM_GET_RUN_STATE, &mut state)?;
        }
        Ok(state)
    }

    /// Executes the guest by running the virtual CPU.
    ///
    /// Blocks the calling thread until the vCPU returns execution,
    /// and returns the reason for exiting ([`VmExit`]).
    ///
    /// When `exit_when_consistent` is asserted, it will instruct the in-kernel
    /// logic to force a [`VmExitKind::Bogus`] exit when the vCPU reaches a
    /// consistent state.  Other exit conditions, such as pending instruction
    /// emulation will take precedence until they are resolved.
    pub fn run(
        &self,
        entry: &VmEntry,
        exit_when_consistent: bool,
    ) -> Result<VmExit> {
        let mut exit: bhyve_api::vm_exit = Default::default();
        let mut entry = entry.to_raw(self.id, &mut exit);

        if exit_when_consistent {
            if self.hdl.api_version()? >= bhyve_api::ApiVersion::V15.into() {
                entry.cmd |=
                    bhyve_api::vm_entry_cmds::VEC_FLAG_EXIT_CONSISTENT as u32;
            } else {
                // On older platforms without EXIT_CONSISTENT, we may spend more
                // time inside VM_RUN than desired, but there is little else
                // that can be done.
            }
        }
        probes::vm_entry!(|| (self.id as u32));
        let _res = unsafe { self.hdl.ioctl(bhyve_api::VM_RUN, &mut entry)? };
        probes::vm_exit!(|| (self.id as u32, exit.rip, exit.exitcode as u32));

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
        unsafe {
            self.hdl.ioctl(bhyve_api::VM_GET_REGISTER, &mut regcmd)?;
        }
        Ok(())
    }

    /// Emit a barrier `Fn`, suitable for use as a
    /// [`TaskHdl`](tasks::TaskHdl) notifier to kick a vCPU out of VMM
    /// context so it undergo state changes in userspace.
    pub fn barrier_fn(self: &Arc<Self>) -> Box<tasks::NotifyFn> {
        let wake_ref = Arc::downgrade(self);
        Box::new(move || {
            if let Some(vcpu) = wake_ref.upgrade() {
                let _ = vcpu.barrier();
            }
        })
    }

    /// Send a Non Maskable Interrupt (NMI) to the vcpu.
    pub fn inject_nmi(&self) -> Result<()> {
        let mut vm_nmi = bhyve_api::vm_nmi { cpuid: self.cpuid() };
        unsafe { self.hdl.ioctl(bhyve_api::VM_INJECT_NMI, &mut vm_nmi) }
    }

    /// Process [`VmExit`] in the context of this vCPU, emitting a [`VmEntry`]
    /// if the parameters of the exit were such that they could be handled.
    pub fn process_vmexit(&self, exit: &VmExit) -> Option<VmEntry> {
        match exit.kind {
            VmExitKind::Bogus => Some(VmEntry::Run),
            VmExitKind::ReqIdle => {
                // another thread came in to use this vCPU it is likely to push
                // us out for a barrier
                Some(VmEntry::Run)
            }
            VmExitKind::Inout(io) => match io {
                InoutReq::Out(io, val) => self
                    .bus_pio
                    .handle_out(io.port, io.bytes, val)
                    .map(|_| VmEntry::InoutFulfill(InoutRes::Out(io)))
                    .ok(),
                InoutReq::In(io) => self
                    .bus_pio
                    .handle_in(io.port, io.bytes)
                    .map(|val| VmEntry::InoutFulfill(InoutRes::In(io, val)))
                    .ok(),
            },
            VmExitKind::Mmio(mmio) => match mmio {
                MmioReq::Read(read) => self
                    .bus_mmio
                    .handle_read(read.addr as usize, read.bytes)
                    .map(|val| {
                        VmEntry::MmioFulfill(MmioRes::Read(MmioReadRes {
                            addr: read.addr,
                            bytes: read.bytes,
                            data: val,
                        }))
                    })
                    .ok(),
                MmioReq::Write(write) => self
                    .bus_mmio
                    .handle_write(write.addr as usize, write.bytes, write.data)
                    .map(|_| {
                        VmEntry::MmioFulfill(MmioRes::Write(MmioWriteRes {
                            addr: write.addr,
                            bytes: write.bytes,
                        }))
                    })
                    .ok(),
            },
            VmExitKind::Rdmsr(_) | VmExitKind::Wrmsr(_, _) => {
                // Leave it to the caller to emulate MSRs unhandled by the kernel
                None
            }
            VmExitKind::Debug => {
                // Until there is an interface to delay until a vCPU is no
                // longer under control of the debugger, we have no choice but
                // attempt reentry (and probably spin until the debugger is
                // detached from this vCPU).
                Some(VmEntry::Run)
            }
            VmExitKind::Suspended(_) => None,

            VmExitKind::InstEmul(_)
            | VmExitKind::Paging(_, _)
            | VmExitKind::VmxError(_)
            | VmExitKind::SvmError(_) => None,
            _ => None,
        }
    }
}

impl Entity for Vcpu {
    fn type_name(&self) -> &'static str {
        "bhyve-vcpu"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Multi(self)
    }

    // The consumer is expected to handle run/pause/halt events directly, since
    // the vCPUs are mostly likely to be driven in manner separate from the
    // other emulated devices.
}
impl MigrateMulti for Vcpu {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> std::result::Result<(), MigrateStateError> {
        output.push(migrate::VcpuRunStateV1::read(self)?.into())?;
        output.push(migrate::VcpuGpRegsV1::read(self)?.into())?;
        output.push(migrate::VcpuCtrlRegsV1::read(self)?.into())?;
        output.push(migrate::VcpuDbgRegsV1::read(self)?.into())?;
        output.push(migrate::VcpuSegRegsV1::read(self)?.into())?;
        output.push(migrate::VcpuMsrsV1::read(self)?.into())?;
        output.push(migrate::FpuStateV1::read(self)?.into())?;
        output.push(migrate::LapicV1::read(self)?.into())?;
        output.push(migrate::CpuidV1::read(self)?.into())?;

        Ok(())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> std::result::Result<(), MigrateStateError> {
        let run_state: migrate::VcpuRunStateV1 = offer.take()?;
        let gp_regs: migrate::VcpuGpRegsV1 = offer.take()?;
        let ctrl_regs: migrate::VcpuCtrlRegsV1 = offer.take()?;
        let dbg_regs: migrate::VcpuDbgRegsV1 = offer.take()?;
        let seg_regs: migrate::VcpuSegRegsV1 = offer.take()?;
        let ms_regs: migrate::VcpuMsrsV1 = offer.take()?;
        let fpu: migrate::FpuStateV1 = offer.take()?;
        let lapic: migrate::LapicV1 = offer.take()?;
        let cpuid: migrate::CpuidV1 = offer.take()?;

        run_state.write(self)?;
        gp_regs.write(self)?;
        ctrl_regs.write(self)?;
        dbg_regs.write(self)?;
        seg_regs.write(self)?;
        ms_regs.write(self)?;
        fpu.write(self)?;
        lapic.write(self)?;
        cpuid.write(self)?;

        Ok(())
    }
}

pub mod migrate {
    use std::io::Result;
    use std::{convert::TryInto, io};

    use super::Vcpu;
    use crate::cpuid;
    use crate::migrate::*;

    use bhyve_api::{vdi_field_entry_v1, vm_reg_name};
    use serde::{Deserialize, Serialize};

    pub(super) trait VcpuReadWrite: Sized {
        fn read(vcpu: &Vcpu) -> Result<Self>;
        fn write(self, vcpu: &Vcpu) -> Result<()>;
    }

    #[derive(Clone, Default, Deserialize, Serialize)]
    pub struct VcpuRunStateV1 {
        pub run_state: u32,
        pub sipi_vector: u8,

        pub intr_shadow: bool,
        pub pending_nmi: bool,
        pub pending_extint: bool,
        pub pending_exception: u64,
        pub pending_intinfo: u64,
    }
    impl Schema<'_> for VcpuRunStateV1 {
        fn id() -> SchemaId {
            ("bhyve-x86-vcpu-runstate", 1)
        }
    }

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct VcpuGpRegsV1 {
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
    impl Schema<'_> for VcpuGpRegsV1 {
        fn id() -> SchemaId {
            ("bhyve-x86-vcpu-gpregs", 1)
        }
    }

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct VcpuCtrlRegsV1 {
        pub cr0: u64,
        pub cr2: u64,
        pub cr3: u64,
        pub cr4: u64,
        pub xcr0: u64,

        /// EFER MSR contents
        ///
        /// We count it among the control registers, rather than the rest of the
        /// MSRs, because of its involvement in configuring long mode.
        pub efer: u64,
    }
    impl Schema<'_> for VcpuCtrlRegsV1 {
        fn id() -> SchemaId {
            ("bhyve-x86-vcpu-ctrlregs", 1)
        }
    }

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct VcpuDbgRegsV1 {
        pub dr0: u64,
        pub dr1: u64,
        pub dr2: u64,
        pub dr3: u64,
        pub dr6: u64,
        pub dr7: u64,
        /// DEBUGCTL MSR
        pub debugctl: u64,
    }
    impl Schema<'_> for VcpuDbgRegsV1 {
        fn id() -> SchemaId {
            ("bhyve-x86-vcpu-dbgregs", 1)
        }
    }

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct VcpuSegRegsV1 {
        pub cs: SegDesc,
        pub ds: SegDesc,
        pub es: SegDesc,
        pub fs: SegDesc,
        pub gs: SegDesc,
        pub ss: SegDesc,
        pub gdtr: SegDesc,
        pub idtr: SegDesc,
        pub ldtr: SegDesc,
        pub tr: SegDesc,
    }
    impl Schema<'_> for VcpuSegRegsV1 {
        fn id() -> SchemaId {
            ("bhyve-x86-vcpu-segregs", 1)
        }
    }

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct SegDesc {
        pub base: u64,
        pub limit: u32,
        pub access: u32,
        pub selector: u16,
    }

    #[derive(Clone, Default, Deserialize, Serialize)]
    pub struct VcpuMsrsV1(Vec<MsrEntry>);
    impl Schema<'_> for VcpuMsrsV1 {
        fn id() -> SchemaId {
            ("bhyve-x86-cpu-msregs", 1)
        }
    }

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct MsrEntry {
        pub ident: u32,
        pub value: u64,
    }

    #[derive(Clone, Default, Deserialize, Serialize)]
    pub struct FpuStateV1 {
        pub blob: Vec<u8>,
    }
    impl Schema<'_> for FpuStateV1 {
        fn id() -> SchemaId {
            ("bhyve-x86-cpu-fpu", 1)
        }
    }

    #[derive(Clone, Default, Deserialize, Serialize)]
    pub struct LapicV1 {
        pub page: LapicPageV1,
        pub msr_apicbase: u64,
        pub timer_target: i64,
        pub esr_pending: u32,
    }
    impl Schema<'_> for LapicV1 {
        fn id() -> SchemaId {
            ("bhyve-x86-lapic", 1)
        }
    }

    #[derive(Clone, Default, Deserialize, Serialize)]
    pub struct LapicPageV1 {
        pub id: u32,
        pub version: u32,
        pub tpr: u32,
        pub apr: u32,
        pub ldr: u32,
        pub dfr: u32,
        pub svr: u32,
        pub isr: [u32; 8],
        pub tmr: [u32; 8],
        pub irr: [u32; 8],
        pub esr: u32,
        pub lvt_cmci: u32,
        pub icr: u64,
        pub lvt_timer: u32,
        pub lvt_thermal: u32,
        pub lvt_pcint: u32,
        pub lvt_lint0: u32,
        pub lvt_lint1: u32,
        pub lvt_error: u32,
        pub icr_timer: u32,
        pub dcr_timer: u32,
    }

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct CpuidEntV1 {
        pub func: u32,
        pub idx: Option<u32>,
        pub data: [u32; 4],
    }
    impl From<CpuidEntV1> for (cpuid::Ident, cpuid::Entry) {
        fn from(value: CpuidEntV1) -> Self {
            (
                cpuid::Ident(value.func, value.idx),
                cpuid::Entry {
                    eax: value.data[0],
                    ebx: value.data[1],
                    ecx: value.data[2],
                    edx: value.data[3],
                },
            )
        }
    }

    #[derive(Copy, Clone, Deserialize, Serialize)]
    #[serde(rename_all = "lowercase")]
    pub enum CpuidVendorV1 {
        Amd,
        Intel,
    }
    impl From<cpuid::VendorKind> for CpuidVendorV1 {
        fn from(value: cpuid::VendorKind) -> Self {
            match value {
                cpuid::VendorKind::Amd => Self::Amd,
                cpuid::VendorKind::Intel => Self::Intel,
            }
        }
    }
    impl From<CpuidVendorV1> for cpuid::VendorKind {
        fn from(value: CpuidVendorV1) -> Self {
            match value {
                CpuidVendorV1::Amd => Self::Amd,
                CpuidVendorV1::Intel => Self::Intel,
            }
        }
    }

    #[derive(Clone, Deserialize, Serialize)]
    pub struct CpuidV1 {
        pub vendor: CpuidVendorV1,
        pub entries: Vec<CpuidEntV1>,
    }
    impl Schema<'_> for CpuidV1 {
        fn id() -> SchemaId {
            ("bhyve-x86-cpuid", 1)
        }
    }
    impl From<cpuid::Set> for CpuidV1 {
        fn from(value: cpuid::Set) -> Self {
            let vendor = value.vendor.into();
            let entries: Vec<_> = value
                .iter()
                .map(|(k, v)| CpuidEntV1 {
                    func: k.0,
                    idx: k.1,
                    data: [v.eax, v.ebx, v.ecx, v.edx],
                })
                .collect();
            CpuidV1 { vendor, entries }
        }
    }
    impl From<CpuidV1> for cpuid::Set {
        fn from(value: CpuidV1) -> Self {
            let mut set = cpuid::Set::new(value.vendor.into());
            for item in value.entries {
                let (ident, value) = item.into();
                set.insert(ident, value);
            }
            set
        }
    }

    impl From<(bhyve_api::seg_desc, u16)> for SegDesc {
        fn from(value: (bhyve_api::seg_desc, u16)) -> Self {
            let (desc, selector) = value;
            Self {
                base: desc.base,
                limit: desc.limit,
                access: desc.access,
                selector,
            }
        }
    }
    impl Into<(bhyve_api::seg_desc, u16)> for SegDesc {
        fn into(self) -> (bhyve_api::seg_desc, u16) {
            (
                bhyve_api::seg_desc {
                    base: self.base,
                    limit: self.limit,
                    access: self.access,
                },
                self.selector,
            )
        }
    }

    impl From<vdi_field_entry_v1> for MsrEntry {
        fn from(raw: vdi_field_entry_v1) -> Self {
            Self { ident: raw.vfe_ident, value: raw.vfe_value }
        }
    }
    impl From<MsrEntry> for vdi_field_entry_v1 {
        fn from(entry: MsrEntry) -> Self {
            vdi_field_entry_v1::new(entry.ident, entry.value)
        }
    }

    impl From<bhyve_api::vdi_lapic_v1> for LapicV1 {
        fn from(value: bhyve_api::vdi_lapic_v1) -> Self {
            Self {
                page: value.vl_lapic.into(),
                msr_apicbase: value.vl_msr_apicbase,
                timer_target: value.vl_timer_target,
                esr_pending: value.vl_esr_pending,
            }
        }
    }
    impl From<LapicV1> for bhyve_api::vdi_lapic_v1 {
        fn from(value: LapicV1) -> Self {
            bhyve_api::vdi_lapic_v1 {
                vl_lapic: value.page.into(),
                vl_msr_apicbase: value.msr_apicbase,
                vl_timer_target: value.timer_target,
                vl_esr_pending: value.esr_pending,
            }
        }
    }
    impl From<bhyve_api::vdi_lapic_page_v1> for LapicPageV1 {
        fn from(value: bhyve_api::vdi_lapic_page_v1) -> Self {
            Self {
                id: value.vlp_id,
                version: value.vlp_version,
                tpr: value.vlp_tpr,
                apr: value.vlp_apr,
                ldr: value.vlp_ldr,
                dfr: value.vlp_dfr,
                svr: value.vlp_svr,
                isr: value.vlp_isr,
                tmr: value.vlp_tmr,
                irr: value.vlp_irr,
                esr: value.vlp_esr,
                lvt_cmci: value.vlp_lvt_cmci,
                icr: value.vlp_icr,
                lvt_timer: value.vlp_lvt_timer,
                lvt_thermal: value.vlp_lvt_thermal,
                lvt_pcint: value.vlp_lvt_pcint,
                lvt_lint0: value.vlp_lvt_lint0,
                lvt_lint1: value.vlp_lvt_lint1,
                lvt_error: value.vlp_lvt_error,
                icr_timer: value.vlp_icr_timer,
                dcr_timer: value.vlp_dcr_timer,
            }
        }
    }
    impl From<LapicPageV1> for bhyve_api::vdi_lapic_page_v1 {
        fn from(value: LapicPageV1) -> Self {
            bhyve_api::vdi_lapic_page_v1 {
                vlp_id: value.id,
                vlp_version: value.version,
                vlp_tpr: value.tpr,
                vlp_apr: value.apr,
                vlp_ldr: value.ldr,
                vlp_dfr: value.dfr,
                vlp_svr: value.svr,
                vlp_isr: value.isr,
                vlp_tmr: value.tmr,
                vlp_irr: value.irr,
                vlp_esr: value.esr,
                vlp_lvt_cmci: value.lvt_cmci,
                vlp_icr: value.icr,
                vlp_lvt_timer: value.lvt_timer,
                vlp_lvt_thermal: value.lvt_thermal,
                vlp_lvt_pcint: value.lvt_pcint,
                vlp_lvt_lint0: value.lvt_lint0,
                vlp_lvt_lint1: value.lvt_lint1,
                vlp_lvt_error: value.lvt_error,
                vlp_icr_timer: value.icr_timer,
                vlp_dcr_timer: value.dcr_timer,
            }
        }
    }

    impl VcpuReadWrite for VcpuRunStateV1 {
        fn read(vcpu: &Vcpu) -> Result<Self> {
            let run_state = vcpu.get_run_state()?;

            let vmm_arch: Vec<bhyve_api::vdi_field_entry_v1> = vcpu
                .hdl
                .data_op(bhyve_api::VDC_VMM_ARCH, 1)
                .for_vcpu(vcpu.id)
                .read_all()?;

            // Load all of the pending interrupt/exception state
            //
            // If illumos#15143 support is missing, none of these fields will be
            // present, so the values will remain false/zeroed.  Such an outcome
            // is fine for now.
            let (
                mut pending_nmi,
                mut pending_extint,
                mut pending_exception,
                mut pending_intinfo,
            ) = (false, false, 0, 0);
            for ent in vmm_arch.iter() {
                match ent.vfe_ident {
                    bhyve_api::VAI_PEND_NMI => pending_nmi = ent.vfe_value != 0,
                    bhyve_api::VAI_PEND_EXTINT => {
                        pending_extint = ent.vfe_value != 0
                    }
                    bhyve_api::VAI_PEND_EXCP => {
                        pending_exception = ent.vfe_value
                    }
                    bhyve_api::VAI_PEND_INTINFO => {
                        pending_intinfo = ent.vfe_value
                    }
                    _ => {}
                }
            }
            let intr_shadow =
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_INTR_SHADOW)? != 0;

            Ok(Self {
                run_state: run_state.state,
                sipi_vector: run_state.sipi_vector,
                intr_shadow,
                pending_nmi,
                pending_extint,
                pending_exception,
                pending_intinfo,
            })
        }

        fn write(self, vcpu: &Vcpu) -> Result<()> {
            vcpu.set_run_state(self.run_state, Some(self.sipi_vector))?;
            vcpu.set_reg(
                vm_reg_name::VM_REG_GUEST_INTR_SHADOW,
                self.intr_shadow as u64,
            )?;

            let ents = [
                vdi_field_entry_v1::new(
                    bhyve_api::VAI_PEND_NMI,
                    self.pending_nmi as u64,
                ),
                vdi_field_entry_v1::new(
                    bhyve_api::VAI_PEND_EXTINT,
                    self.pending_extint as u64,
                ),
                vdi_field_entry_v1::new(
                    bhyve_api::VAI_PEND_EXCP,
                    self.pending_exception,
                ),
                vdi_field_entry_v1::new(
                    bhyve_api::VAI_PEND_INTINFO,
                    self.pending_intinfo,
                ),
            ];

            // Do not attempt to import interrupt/exception state unless there
            // is proper support for it on the host we are running upon.
            //
            // When hosts with illumos#15143 integrated become common, the
            // overall required version for propolis can grow to encompass V10
            // and this check can be elided.
            if bhyve_api::api_version()? >= bhyve_api::ApiVersion::V10 as u32 {
                vcpu.hdl
                    .data_op(bhyve_api::VDC_VMM_ARCH, 1)
                    .for_vcpu(vcpu.id)
                    .write_many(&ents)?;
            }

            Ok(())
        }
    }

    impl VcpuReadWrite for VcpuGpRegsV1 {
        fn read(vcpu: &Vcpu) -> Result<Self> {
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

        fn write(self, vcpu: &Vcpu) -> Result<()> {
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RAX, self.rax)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RCX, self.rcx)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RDX, self.rdx)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RBX, self.rbx)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RSP, self.rsp)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RBP, self.rbp)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RSI, self.rsi)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RDI, self.rdi)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_R8, self.r8)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_R9, self.r9)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_R10, self.r10)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_R11, self.r11)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_R12, self.r12)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_R13, self.r13)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_R14, self.r14)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_R15, self.r15)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RIP, self.rip)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RFLAGS, self.rflags)?;
            Ok(())
        }
    }

    impl VcpuReadWrite for VcpuCtrlRegsV1 {
        fn read(vcpu: &Vcpu) -> Result<Self> {
            Ok(Self {
                cr0: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_CR0)?,
                cr2: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_CR2)?,
                cr3: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_CR3)?,
                cr4: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_CR4)?,
                efer: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_EFER)?,
                xcr0: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_XCR0)?,
            })
        }

        fn write(self, vcpu: &Vcpu) -> Result<()> {
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_CR0, self.cr0)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_CR2, self.cr2)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_CR3, self.cr3)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_CR4, self.cr4)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_EFER, self.efer)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_XCR0, self.xcr0)?;
            Ok(())
        }
    }
    impl VcpuReadWrite for VcpuDbgRegsV1 {
        fn read(vcpu: &Vcpu) -> Result<Self> {
            Ok(Self {
                dr0: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR0)?,
                dr1: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR1)?,
                dr2: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR2)?,
                dr3: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR3)?,
                dr6: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR6)?,
                dr7: vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DR7)?,
                // TODO: populate from MSR
                debugctl: 0,
            })
        }

        fn write(self, vcpu: &Vcpu) -> Result<()> {
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_DR0, self.dr0)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_DR1, self.dr1)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_DR2, self.dr2)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_DR3, self.dr3)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_DR6, self.dr6)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_DR7, self.dr7)?;
            // TODO: set debugctl MSR
            Ok(())
        }
    }

    impl VcpuReadWrite for VcpuSegRegsV1 {
        fn read(vcpu: &Vcpu) -> Result<Self> {
            let cs = SegDesc::from((
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_CS)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_CS)? as u16,
            ));
            let ds = SegDesc::from((
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_DS)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_DS)? as u16,
            ));
            let es = SegDesc::from((
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_ES)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_ES)? as u16,
            ));
            let fs = SegDesc::from((
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_FS)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_FS)? as u16,
            ));
            let gs = SegDesc::from((
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_GS)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_GS)? as u16,
            ));
            let ss = SegDesc::from((
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_SS)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_SS)? as u16,
            ));
            let gdtr = SegDesc::from((
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_GDTR)?,
                // GDT has no selector register
                0,
            ));
            let idtr = SegDesc::from((
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_IDTR)?,
                // IDT has no selector register
                0,
            ));
            let ldtr = SegDesc::from((
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_LDTR)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_LDTR)? as u16,
            ));
            let tr = SegDesc::from((
                vcpu.get_segreg(vm_reg_name::VM_REG_GUEST_TR)?,
                vcpu.get_reg(vm_reg_name::VM_REG_GUEST_TR)? as u16,
            ));
            Ok(Self { cs, ds, es, fs, gs, ss, gdtr, idtr, ldtr, tr })
        }

        fn write(self, vcpu: &Vcpu) -> Result<()> {
            let (cs, css) = self.cs.into();
            vcpu.set_segreg(vm_reg_name::VM_REG_GUEST_CS, &cs)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_CS, css.into())?;

            let (ds, dss) = self.ds.into();
            vcpu.set_segreg(vm_reg_name::VM_REG_GUEST_DS, &ds)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_DS, dss.into())?;

            let (es, ess) = self.es.into();
            vcpu.set_segreg(vm_reg_name::VM_REG_GUEST_ES, &es)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_ES, ess.into())?;

            let (fs, fss) = self.fs.into();
            vcpu.set_segreg(vm_reg_name::VM_REG_GUEST_FS, &fs)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_FS, fss.into())?;

            let (gs, gss) = self.gs.into();
            vcpu.set_segreg(vm_reg_name::VM_REG_GUEST_GS, &gs)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_GS, gss.into())?;

            let (ss, sss) = self.ss.into();
            vcpu.set_segreg(vm_reg_name::VM_REG_GUEST_SS, &ss)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_SS, sss.into())?;

            let (gdtr, _) = self.gdtr.into();
            vcpu.set_segreg(vm_reg_name::VM_REG_GUEST_GDTR, &gdtr)?;

            let (idtr, _) = self.idtr.into();
            vcpu.set_segreg(vm_reg_name::VM_REG_GUEST_IDTR, &idtr)?;

            let (ldtr, ldtrs) = self.ldtr.into();
            vcpu.set_segreg(vm_reg_name::VM_REG_GUEST_LDTR, &ldtr)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_LDTR, ldtrs.into())?;

            let (tr, trs) = self.tr.into();
            vcpu.set_segreg(vm_reg_name::VM_REG_GUEST_TR, &tr)?;
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_TR, trs.into())?;
            Ok(())
        }
    }
    impl VcpuMsrsV1 {
        const fn valid_msr(ident: u32) -> bool {
            use super::bits::*;

            match ident {
                // EFER is held in CtrlRegs
                MSR_EFER => false,
                // DEBUGCTL is held in DbgRegs
                MSR_DEBUGCTL => false,

                _ => true,
            }
        }
    }

    impl VcpuReadWrite for VcpuMsrsV1 {
        fn read(vcpu: &Vcpu) -> Result<Self> {
            let raw_msrs: Vec<bhyve_api::vdi_field_entry_v1> = vcpu
                .hdl
                .data_op(bhyve_api::VDC_MSR, 1)
                .for_vcpu(vcpu.id)
                .read_all()?;

            let mut filtered: Vec<MsrEntry> = raw_msrs
                .into_iter()
                .filter_map(|ent| {
                    if Self::valid_msr(ent.vfe_ident) {
                        Some(ent.into())
                    } else {
                        None
                    }
                })
                .collect();

            // Sort the output to make it more readable in case someone happens
            // to be inspecting the device payloads
            filtered.sort_unstable_by_key(|v| v.ident);

            Ok(Self(filtered))
        }

        fn write(self, vcpu: &Vcpu) -> Result<()> {
            let raw_msrs: Vec<bhyve_api::vdi_field_entry_v1> = self
                .0
                .into_iter()
                .filter_map(|ent| {
                    // belt-and-suspenders verification that provided MSRs are
                    // acceptable for loading into the kernel vmm
                    if Self::valid_msr(ent.ident) {
                        Some(ent.into())
                    } else {
                        None
                    }
                })
                .collect();

            vcpu.hdl
                .data_op(bhyve_api::VDC_MSR, 1)
                .for_vcpu(vcpu.id)
                .write_many(&raw_msrs)?;

            Ok(())
        }
    }

    impl VcpuReadWrite for FpuStateV1 {
        fn read(vcpu: &Vcpu) -> Result<Self> {
            let mut fpu_area_desc = bhyve_api::vm_fpu_desc::default();

            unsafe {
                vcpu.hdl
                    .ioctl(bhyve_api::VM_DESC_FPU_AREA, &mut fpu_area_desc)?;
            }
            let len = fpu_area_desc.vfd_req_size as usize;
            let mut fpu = Vec::with_capacity(len);
            fpu.resize_with(len, u8::default);

            let mut fpu_req = bhyve_api::vm_fpu_state {
                vcpuid: vcpu.cpuid(),
                buf: fpu.as_mut_ptr() as *mut libc::c_void,
                len: fpu_area_desc.vfd_req_size,
            };
            unsafe {
                vcpu.hdl.ioctl(bhyve_api::VM_GET_FPU, &mut fpu_req)?;
            }

            Ok(Self { blob: fpu })
        }

        fn write(mut self, vcpu: &Vcpu) -> Result<()> {
            let mut fpu_req = bhyve_api::vm_fpu_state {
                vcpuid: vcpu.cpuid(),
                buf: self.blob.as_mut_ptr() as *mut _,
                len: self.blob.len().try_into().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::Other,
                        "fpu blob size too large",
                    )
                })?,
            };
            unsafe {
                vcpu.hdl.ioctl(bhyve_api::VM_SET_FPU, &mut fpu_req)?;
            }
            Ok(())
        }
    }
    impl VcpuReadWrite for LapicV1 {
        fn read(vcpu: &Vcpu) -> Result<Self> {
            let vdi = vcpu
                .hdl
                .data_op(bhyve_api::VDC_LAPIC, 1)
                .for_vcpu(vcpu.id)
                .read::<bhyve_api::vdi_lapic_v1>()?;

            Ok(vdi.into())
        }
        fn write(self, vcpu: &Vcpu) -> Result<()> {
            vcpu.hdl
                .data_op(bhyve_api::VDC_LAPIC, 1)
                .for_vcpu(vcpu.id)
                .write::<bhyve_api::vdi_lapic_v1>(&self.into())?;

            Ok(())
        }
    }
    impl VcpuReadWrite for CpuidV1 {
        fn read(vcpu: &Vcpu) -> Result<Self> {
            Ok(vcpu.get_cpuid()?.into())
        }

        fn write(self, vcpu: &Vcpu) -> Result<()> {
            vcpu.set_cpuid(self.into())
        }
    }
}

mod bits {
    pub const MSR_DEBUGCTL: u32 = 0x1d9;
    pub const MSR_EFER: u32 = 0xc0000080;
}
