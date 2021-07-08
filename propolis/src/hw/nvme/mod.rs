use std::convert::TryInto;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::pci;
use crate::util::regmap::RegMap;

use lazy_static::lazy_static;
use thiserror::Error;

pub use ns::NvmeNs;

mod admin;
mod bits;
mod cmds;
mod ns;
mod queue;

use bits::*;
use queue::{CompQueue, SubQueue};

const NVME_MSIX_COUNT: u16 = 1024;

#[derive(Debug, Error)]
enum NvmeError {
    #[error("the completion queue specified ({0}) is invalid")]
    InvalidCompQueue(u16),

    #[error("the submission queue specified ({0}) is invalid")]
    InvalidSubQueue(u16),

    #[error("the completition queue specified ({0}) already exists")]
    CompQueueAlreadyExists(u16),

    #[error("the submission queue specified ({0}) already exists")]
    SubQueueAlreadyExists(u16),

    #[error("failed to create queue: {0}")]
    QueueCreateErr(#[from] queue::QueueCreateErr),

    #[error("the MSI-X interrupt handle is unavailable")]
    MsixHdlUnavailable,

    #[error("failed to parse command: {0}")]
    CommandParseErr(#[from] cmds::ParseErr),
}

#[derive(Debug, Default)]
struct CtrlState {
    enabled: bool,
    ready: bool,
    admin_sq_base: u64,
    admin_cq_base: u64,
    admin_sq_size: u16,
    admin_cq_size: u16,
}

/// The max number of completion or submission queues we support.
const MAX_NUM_QUEUES: usize = 16;

/// The Admin Completion and Submission are always ID 0
const ADMIN_QUEUE_ID: u16 = 0;

struct NvmeCtrl {
    /// Internal NVMe Controller state
    ctrl: CtrlState,

    /// MSI-X Interrupt Handle to signal VM
    msix_hdl: Option<pci::MsixHdl>,

    /// The list of Completion Queues handled by the controller
    cqs: [Option<Arc<Mutex<CompQueue>>>; MAX_NUM_QUEUES],

    /// The list of Submission Queues handled by the controller
    sqs: [Option<Arc<Mutex<SubQueue>>>; MAX_NUM_QUEUES],

    // TODO: Support more than 1 namespace
    ns: ns::NvmeNs,

    /// The PCI Vendor ID the Controller is initialized with
    vendor_id: u16,
}

impl NvmeCtrl {
    /// Creates the admin completion and submission queues.
    ///
    /// Admin queues are always created with `cqid`/`sqid` `0`.
    fn create_admin_queues(&mut self, ctx: &DispCtx) -> Result<(), NvmeError> {
        self.create_cq(
            ADMIN_QUEUE_ID,
            0, // Admin CQ uses interrupt vector 0
            GuestAddr(self.ctrl.admin_cq_base),
            self.ctrl.admin_cq_size as u32,
            ctx,
        )?;
        self.create_sq(
            ADMIN_QUEUE_ID,
            ADMIN_QUEUE_ID,
            GuestAddr(self.ctrl.admin_sq_base),
            self.ctrl.admin_sq_size as u32,
            ctx,
        )?;
        Ok(())
    }

    /// Creates and stores a new completion queue ([`CompQueue`]) for the controller.
    ///
    /// The specified `cqid` must not already be in use by another completion queue.
    fn create_cq(
        &mut self,
        cqid: u16,
        iv: u16,
        base: GuestAddr,
        size: u32,
        ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        if (cqid as usize) >= MAX_NUM_QUEUES {
            return Err(NvmeError::InvalidCompQueue(cqid));
        }
        if self.cqs[cqid as usize].is_some() {
            return Err(NvmeError::CompQueueAlreadyExists(cqid));
        }
        let msix_hdl = self
            .msix_hdl
            .as_ref()
            .ok_or(NvmeError::MsixHdlUnavailable)?
            .clone();
        let cq = CompQueue::new(cqid, iv, size, base, ctx, msix_hdl)?;
        self.cqs[cqid as usize] = Some(Arc::new(Mutex::new(cq)));
        Ok(())
    }

    /// Creates and stores a new submission queue ([`SubQueue`]) for the controller.
    ///
    /// The specified `sqid` must not already be in use by another submission queue.
    /// The corresponding completion queue specified (`cqid`) must already exist.
    fn create_sq(
        &mut self,
        sqid: u16,
        cqid: u16,
        base: GuestAddr,
        size: u32,
        ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        if (sqid as usize) >= MAX_NUM_QUEUES {
            return Err(NvmeError::InvalidSubQueue(cqid));
        }
        if (cqid as usize) >= MAX_NUM_QUEUES
            || self.cqs[cqid as usize].is_none()
        {
            return Err(NvmeError::InvalidCompQueue(cqid));
        }
        if self.sqs[sqid as usize].is_some() {
            return Err(NvmeError::SubQueueAlreadyExists(cqid));
        }
        let sq = SubQueue::new(sqid, cqid, size, base, ctx)?;
        self.sqs[sqid as usize] = Some(Arc::new(Mutex::new(sq)));
        Ok(())
    }

    /// Returns a reference to the [`CompQueue`] which corresponds to the given completion queue id (`cqid`).
    fn get_cq(&self, cqid: u16) -> Result<Arc<Mutex<CompQueue>>, NvmeError> {
        debug_assert!((cqid as usize) < MAX_NUM_QUEUES);
        self.cqs[cqid as usize]
            .as_ref()
            .map(Arc::clone)
            .ok_or(NvmeError::InvalidCompQueue(cqid))
    }

    /// Returns a reference to the [`SubQueue`] which corresponds to the given submission queue id (`cqid`).
    fn get_sq(&self, sqid: u16) -> Result<Arc<Mutex<SubQueue>>, NvmeError> {
        debug_assert!((sqid as usize) < MAX_NUM_QUEUES);
        self.sqs[sqid as usize]
            .as_ref()
            .map(Arc::clone)
            .ok_or(NvmeError::InvalidSubQueue(sqid))
    }

    /// Returns a reference to the Admin [`CompQueue`].
    ///
    /// # Panics
    ///
    /// Panics if the Admin Completion Queue hasn't been created yet.
    fn get_admin_cq(&self) -> Arc<Mutex<CompQueue>> {
        self.get_cq(ADMIN_QUEUE_ID).unwrap()
    }

    /// Returns a reference to the Admin [`SubQueue`].
    ///
    /// # Panics
    ///
    /// Panics if the Admin Submission Queue hasn't been created yet.
    fn get_admin_sq(&self) -> Arc<Mutex<SubQueue>> {
        self.get_sq(ADMIN_QUEUE_ID).unwrap()
    }

    /// Performs a Controller Reset.
    ///
    /// The reset deletes all I/O Submission & Completion Queues, resets
    /// the Admin Submission & Completion Queues, and brings the hardware
    /// to an idle state. The reset does not affect PCI Express registers
    /// nor the Admin Queue registers (AQA, ASQ, or ACQ).  All other
    /// controller registers and internal controller state that are not
    /// persistent across power states) are reset to their default values.
    /// The controller shall ensure that there is no data loss for commands
    /// that have had corresponding completion queue entries posted to an I/O
    /// Completion Queue prior to the reset operation.
    fn reset(&mut self) {
        // TODO: handle any pending commands

        for sq in &mut self.sqs {
            *sq = None;
        }
        for cq in &mut self.cqs {
            *cq = None;
        }
    }
}

pub struct PciNvme {
    state: Mutex<NvmeCtrl>,
}

impl PciNvme {
    pub fn create(
        vendor: u16,
        device: u16,
        ns: NvmeNs,
    ) -> Arc<pci::DeviceInst> {
        let builder = pci::Builder::new(pci::Ident {
            vendor_id: vendor,
            device_id: device,
            sub_vendor_id: vendor,
            sub_device_id: device,
            class: pci::bits::CLASS_STORAGE,
            subclass: pci::bits::SUBCLASS_NVM,
            prog_if: pci::bits::PROGIF_ENTERPRISE_NVMHCI,
            ..Default::default()
        });

        let state = NvmeCtrl {
            ctrl: CtrlState::default(),
            msix_hdl: None,
            cqs: Default::default(),
            sqs: Default::default(),
            vendor_id: vendor,
            ns,
        };

        let nvme = PciNvme { state: Mutex::new(state) };

        builder
            // XXX: add room for doorbells
            .add_bar_mmio64(pci::BarN::BAR0, CONTROLLER_REG_SZ as u64)
            // BAR0/1 are used for the main config and doorbell registers
            // BAR2 is for the optional index/data registers
            // Place MSIX in BAR4 for now
            .add_cap_msix(pci::BarN::BAR4, NVME_MSIX_COUNT)
            .finish(Arc::new(nvme))
    }

    fn ctrlr_cfg_write(
        &self,
        val: u32,
        ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        let mut state = self.state.lock().unwrap();

        if !state.ctrl.enabled {
            // TODO: apply any necessary config changes
        }

        let now_enabled = val & CC_EN != 0;
        if now_enabled && !state.ctrl.enabled {
            state.ctrl.enabled = true;

            // Create the Admin Completion and Submission queues
            state.create_admin_queues(ctx)?;

            state.ctrl.ready = true;
        } else if !now_enabled && state.ctrl.enabled {
            state.ctrl.enabled = false;
            state.ctrl.ready = false;

            state.reset();
        }

        Ok(())
    }
    fn reg_ctrl_read(
        &self,
        id: &CtrlrReg,
        ro: &mut ReadOp,
        _ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        match id {
            CtrlrReg::CtrlrCaps => {
                // MPSMIN = MPSMAX = 0 (4k pages)
                // CCS = 0x1 - NVM command set
                // DSTRD = 0 - standard (32-bit) doorbell stride
                // TO = 0 - 0 * 500ms to wait for controller ready
                // AMS = 0x0 - no additional abitrary mechs (besides RR)
                // CQR = 0x1 - contig queues required for now
                // MQES = 0xfff - 4k (zeros-based)
                ro.write_u64(CAP_CCS | CAP_CQR | 0x0fff);
            }
            CtrlrReg::Version => {
                ro.write_u32(NVME_VER_1_0);
            }

            CtrlrReg::IntrMaskSet | CtrlrReg::IntrMaskClear => {
                // Only MSI-X is exposed for now, so this is undefined
                ro.fill(0);
            }

            CtrlrReg::CtrlrCfg => {
                let state = self.state.lock().unwrap();
                let mut val = if state.ctrl.enabled { 1 } else { 0 };
                val |= 4 << 20 // IOCQES 23:20 - 2^4 = 16 bytes
                | 6 << 16; // IOSQES 19:16 - 2^6 = 64 bytes
                ro.write_u32(val);
            }
            CtrlrReg::CtrlrStatus => {
                let state = self.state.lock().unwrap();
                let mut val = 0;

                if state.ctrl.ready {
                    val |= CSTS_READY;
                }
                ro.write_u32(val);
            }
            CtrlrReg::AdminQueueAttr => {
                let state = self.state.lock().unwrap();
                ro.write_u32(
                    state.ctrl.admin_sq_size as u32
                        | (state.ctrl.admin_cq_size as u32) << 16,
                );
            }
            CtrlrReg::AdminSubQAddr => {
                let state = self.state.lock().unwrap();
                ro.write_u64(state.ctrl.admin_sq_base);
            }
            CtrlrReg::AdminCompQAddr => {
                let state = self.state.lock().unwrap();
                ro.write_u64(state.ctrl.admin_cq_base);
            }
            CtrlrReg::Reserved => {
                ro.fill(0);
            }
            CtrlrReg::DoorBellAdminSQ
            | CtrlrReg::DoorBellAdminCQ
            | CtrlrReg::DoorBellIoSQ1
            | CtrlrReg::DoorBellIoCQ1 => {
                // The host should not read from the doorbells, and the contents
                // can be vendor/implementation specific (in our case, zeroed).
                ro.fill(0);
            }
        }

        Ok(())
    }
    fn reg_ctrl_write(
        &self,
        id: &CtrlrReg,
        wo: &mut WriteOp,
        ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        match id {
            CtrlrReg::CtrlrCaps
            | CtrlrReg::Version
            | CtrlrReg::CtrlrStatus
            | CtrlrReg::Reserved => {
                // Read-only registers
            }
            CtrlrReg::IntrMaskSet | CtrlrReg::IntrMaskClear => {
                // Only MSI-X is exposed for now, so this is undefined
            }

            CtrlrReg::CtrlrCfg => {
                self.ctrlr_cfg_write(wo.read_u32(), ctx)?;
            }
            CtrlrReg::AdminQueueAttr => {
                let mut state = self.state.lock().unwrap();
                if !state.ctrl.enabled {
                    let val = wo.read_u32();
                    // bits 27:16 - ACQS, zeroes-based
                    let compq: u16 = ((val >> 16) & 0xfff) as u16 + 1;
                    // bits 27:16 - ASQS, zeroes-based
                    let subq: u16 = (val & 0xfff) as u16 + 1;

                    state.ctrl.admin_cq_size = compq;
                    state.ctrl.admin_sq_size = subq;
                }
            }
            CtrlrReg::AdminSubQAddr => {
                let mut state = self.state.lock().unwrap();
                if !state.ctrl.enabled {
                    state.ctrl.admin_sq_base = wo.read_u64() & PAGE_MASK as u64;
                }
            }
            CtrlrReg::AdminCompQAddr => {
                let mut state = self.state.lock().unwrap();
                if !state.ctrl.enabled {
                    state.ctrl.admin_cq_base = wo.read_u64() & PAGE_MASK as u64;
                }
            }

            CtrlrReg::DoorBellAdminSQ => {
                let val = wo.read_u32().try_into().unwrap();
                let state = self.state.lock().unwrap();
                let admin_sq = state.get_admin_sq();
                let mut sq = admin_sq.lock().unwrap();
                match sq.notify_tail(val) {
                    Ok(_) => {}
                    Err(_) => todo!("set controller error state"),
                }

                // Process any new SQ entries
                self.process_admin_queue(state, sq, ctx)?;
            }
            CtrlrReg::DoorBellAdminCQ => {
                let val = wo.read_u32().try_into().unwrap();
                let state = self.state.lock().unwrap();
                let admin_cq = state.get_admin_cq();
                let mut cq = admin_cq.lock().unwrap();
                match cq.notify_head(val) {
                    Ok(_) => {}
                    Err(_) => todo!("set controller error state"),
                }
                // TODO: post any entries to the CQ now that it has more space
            }

            CtrlrReg::DoorBellIoSQ1 => {
                let val = wo.read_u32().try_into().unwrap();
                let state = self.state.lock().unwrap();
                // TODO: Support more than 1 I/O queue
                let io_sq = state.get_sq(1)?;
                let mut sq = io_sq.lock().unwrap();
                match sq.notify_tail(val) {
                    Ok(_) => {}
                    Err(_) => todo!("set controller error state"),
                }
                drop(sq);
                self.process_io_queue(state, io_sq, ctx)?;
            }
            CtrlrReg::DoorBellIoCQ1 => {
                let val = wo.read_u32().try_into().unwrap();
                let state = self.state.lock().unwrap();
                // TODO: Support more than 1 I/O queue
                let io_cq = state.get_cq(1)?;
                let mut cq = io_cq.lock().unwrap();
                match cq.notify_head(val) {
                    Ok(_) => {}
                    Err(_) => todo!("set controller error state"),
                }
                // TODO: post any entries to the CQ now that it has more space
            }
        }

        Ok(())
    }

    fn process_admin_queue(
        &self,
        mut state: MutexGuard<NvmeCtrl>,
        mut sq: MutexGuard<SubQueue>,
        ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        // Grab the Admin CQ too
        let admin_cq = state.get_admin_cq();
        let mut cq = admin_cq.lock().unwrap();

        while let Some(sub) = sq.pop(ctx) {
            use cmds::AdminCmd;

            let parsed = AdminCmd::parse(sub);
            if parsed.is_err() {
                // XXX: set controller error state?
                continue;
            }
            let (cmd, _) = parsed.unwrap();
            let comp = match cmd {
                AdminCmd::CreateIOCompQ(cmd) => {
                    state.acmd_create_io_cq(&cmd, ctx)
                }
                AdminCmd::CreateIOSubQ(cmd) => {
                    state.acmd_create_io_sq(&cmd, ctx)
                }
                AdminCmd::GetLogPage(cmd) => state.acmd_get_log_page(&cmd, ctx),
                AdminCmd::Identify(cmd) => state.acmd_identify(&cmd, ctx),
                AdminCmd::SetFeatures(cmd) => {
                    state.acmd_set_features(&cmd, ctx)
                }
                AdminCmd::DeleteIOSubQ(_)
                | AdminCmd::DeleteIOCompQ(_)
                | AdminCmd::Abort
                | AdminCmd::GetFeatures
                | AdminCmd::AsyncEventReq
                | AdminCmd::Unknown(_) => {
                    cmds::Completion::generic_err(bits::STS_INTERNAL_ERR)
                }
            };

            let completion = RawCompletion {
                cdw0: comp.cdw0,
                rsvd: 0,
                sqhd: sq.head(),
                sqid: sq.id(),
                cid: sub.cid(),
                status: comp.status | cq.phase(),
            };
            cq.push(completion, ctx);
        }

        // Notify for any newly added completions
        cq.fire_interrupt(ctx);

        Ok(())
    }

    fn process_io_queue(
        &self,
        state: MutexGuard<NvmeCtrl>,
        io_sq: Arc<Mutex<SubQueue>>,
        ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        let mut sq = io_sq.lock().unwrap();

        // Grab the corresponding CQ
        let io_cq = state.get_cq(sq.cqid())?;

        // Collect all the IO SQ entries
        let mut io_cmds = vec![];
        while let Some(sub) = sq.pop(ctx) {
            // TODO: 1 hardcoded namespace
            assert_eq!(sub.nsid, 1);

            io_cmds.push(sub);
        }

        drop(sq);

        // Queue up said IO entries to the underlying block device
        state.ns.queue_io_cmds(io_cmds, io_cq.clone(), io_sq, ctx)?;

        // Notify for any newly added completions
        let cq = io_cq.lock().unwrap();
        cq.fire_interrupt(ctx);

        Ok(())
    }
}

impl pci::Device for PciNvme {
    fn bar_rw(&self, bar: pci::BarN, mut rwo: RWOp, ctx: &DispCtx) {
        assert_eq!(bar, pci::BarN::BAR0);
        CONTROLLER_REGS.process(&mut rwo, |id, rwo| {
            let res = match rwo {
                RWOp::Read(ro) => self.reg_ctrl_read(id, ro, ctx),
                RWOp::Write(wo) => self.reg_ctrl_write(id, wo, ctx),
            };
            // TODO: is there a better way to report errors
            if let Err(err) = res {
                eprintln!("nvme reg read/write failed: {}", err);
            }
        });
    }

    fn attach(
        &self,
        lintr_pin: Option<pci::INTxPin>,
        msix_hdl: Option<pci::MsixHdl>,
    ) {
        assert!(lintr_pin.is_none());
        assert!(msix_hdl.is_some());
        self.state.lock().unwrap().msix_hdl = msix_hdl;
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum CtrlrReg {
    CtrlrCaps,
    Version,
    IntrMaskSet,
    IntrMaskClear,
    CtrlrCfg,
    CtrlrStatus,
    AdminQueueAttr,
    AdminSubQAddr,
    AdminCompQAddr,
    Reserved,

    DoorBellAdminSQ,
    DoorBellAdminCQ,

    // XXX: Can we coalesce these
    DoorBellIoSQ1,
    DoorBellIoCQ1,
}
// XXX: single IO doorbell for prototype
const CONTROLLER_REG_SZ: usize = 0x2000;
lazy_static! {
    static ref CONTROLLER_REGS: RegMap<CtrlrReg> = {
        let layout = [
            (CtrlrReg::CtrlrCaps, 8),
            (CtrlrReg::Version, 4),
            (CtrlrReg::IntrMaskSet, 4),
            (CtrlrReg::IntrMaskClear, 4),
            (CtrlrReg::CtrlrCfg, 4),
            (CtrlrReg::Reserved, 4),
            (CtrlrReg::CtrlrStatus, 4),
            (CtrlrReg::Reserved, 4),
            (CtrlrReg::AdminQueueAttr, 4),
            (CtrlrReg::AdminSubQAddr, 8),
            (CtrlrReg::AdminCompQAddr, 8),
            (CtrlrReg::Reserved, 0xec8),
            (CtrlrReg::Reserved, 0x100),
            // XXX: hardcode 0 stride for doorbells
            (CtrlrReg::DoorBellAdminSQ, 4),
            (CtrlrReg::DoorBellAdminCQ, 4),
            // XXX: hardcode a single IO doorbell
            (CtrlrReg::DoorBellIoSQ1, 4),
            (CtrlrReg::DoorBellIoCQ1, 4),
            // XXX: pad out to next power of 2
            (CtrlrReg::Reserved, 0x1000 - 16),
        ];
        RegMap::create_packed(
            CONTROLLER_REG_SZ,
            &layout,
            Some(CtrlrReg::Reserved),
        )
    };
}
