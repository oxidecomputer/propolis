use std::convert::TryInto;
use std::mem::size_of;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::dispatch::DispCtx;
use crate::hw::pci;
use crate::util::regmap::RegMap;
use crate::{block, common::*};

use lazy_static::lazy_static;
use thiserror::Error;

mod admin;
mod bits;
mod cmds;
mod queue;
mod requests;

use bits::*;
use queue::{CompQueue, QueueId, SubQueue};

/// The max number of MSI-X interrupts we support
const NVME_MSIX_COUNT: u16 = 1024;

/// Supported block size.
/// TODO: Support more
const BLOCK_SZ: u64 = 512;

/// NVMe errors
#[derive(Debug, Error)]
pub enum NvmeError {
    /// The specified Completion Queue ID did not correspond to a valid Completion Queue
    #[error("the completion queue specified ({0}) is invalid")]
    InvalidCompQueue(QueueId),

    /// The specified Submission Queue ID did not correspond to a valid Completion Queue
    #[error("the submission queue specified ({0}) is invalid")]
    InvalidSubQueue(QueueId),

    /// The specified Completion Queue ID already exists
    #[error("the completition queue specified ({0}) already exists")]
    CompQueueAlreadyExists(QueueId),

    /// The specified Submission Queue ID already exists
    #[error("the submission queue specified ({0}) already exists")]
    SubQueueAlreadyExists(QueueId),

    /// Failed to create Queue
    #[error("failed to create queue: {0}")]
    QueueCreateErr(#[from] queue::QueueCreateErr),

    #[error("failed to update queue: {0}")]
    QueueUpdateError(#[from] queue::QueueUpdateError),

    /// MSI-X Interrupt handle is unavailable
    #[error("the MSI-X interrupt handle is unavailable")]
    MsixHdlUnavailable,

    /// Couln't parse command
    #[error("failed to parse command: {0}")]
    CommandParseErr(#[from] cmds::ParseErr),

    /// Maximum number of namespaces already attached to controller
    #[error("maximum number of namespaces already attached to controller")]
    TooManyNamespaces,

    /// The specified Namespace ID did not correspond to a valid Namespace
    #[error("the namespace specified ({0}) is invalid")]
    InvalidNamespace(u32),
}

/// Internal NVMe Controller State
#[derive(Debug, Default)]
struct CtrlState {
    /// Controller Capabilities
    cap: Capabilities,

    /// Controller Configuration
    cc: Configuration,

    /// Controller Status
    csts: Status,

    /// Admin Queue Attributes
    aqa: AdminQueueAttrs,

    /// The 64-bit Guest address for the Admin Submission Queue
    ///
    /// ASQB
    /// See NVMe 1.0e Section 3.1.8 Offset 28h: ASQ - Admin Submission Queue Base Address
    admin_sq_base: u64,

    /// The 64-bit Guest address for the Admin Completion Queue
    ///
    /// ACQB
    /// See NVMe 1.0e Section 3.1.9 Offset 30h: ACQ - Admin Completion Queue Base Address
    admin_cq_base: u64,
}

/// The max number of completion or submission queues we support.
/// Note: This includes the admin completion/submission queues.
const MAX_NUM_QUEUES: usize = 16;

/// The max number of I/O completion or submission queues we support.
/// Always 1 less than the total number w/ the admin queues.
const MAX_NUM_IO_QUEUES: usize = MAX_NUM_QUEUES - 1;

/// NVMe Controller
struct NvmeCtrl {
    /// Internal NVMe Controller state
    ctrl: CtrlState,

    /// MSI-X Interrupt Handle to signal VM
    msix_hdl: Option<pci::MsixHdl>,

    /// The list of Completion Queues handled by the controller
    cqs: [Option<Arc<CompQueue>>; MAX_NUM_QUEUES],

    /// The list of Submission Queues handled by the controller
    sqs: [Option<Arc<SubQueue>>; MAX_NUM_QUEUES],

    /// The Identify structure returned for Identify controller commands
    ctrl_ident: IdentifyController,

    /// The Identify structure returned for Identify namespace commands
    ns_ident: IdentifyNamespace,

    /// Underlying Block Device info
    binfo: block::DeviceInfo,
}

impl NvmeCtrl {
    /// Creates the admin completion and submission queues.
    ///
    /// Admin queues are always created with `cqid`/`sqid` `0`.
    fn create_admin_queues(&mut self, ctx: &DispCtx) -> Result<(), NvmeError> {
        // Admin CQ uses interrupt vector 0 (See NVMe 1.0e Section 3.1.9 ACQ)
        self.create_cq(
            queue::ADMIN_QUEUE_ID,
            0,
            GuestAddr(self.ctrl.admin_cq_base),
            // Convert from 0's based
            self.ctrl.aqa.acqs() as u32 + 1,
            ctx,
        )?;
        self.create_sq(
            queue::ADMIN_QUEUE_ID,
            queue::ADMIN_QUEUE_ID,
            GuestAddr(self.ctrl.admin_sq_base),
            // Convert from 0's based
            self.ctrl.aqa.asqs() as u32 + 1,
            ctx,
        )?;
        Ok(())
    }

    /// Creates and stores a new completion queue ([`CompQueue`]) for the controller.
    ///
    /// The specified `cqid` must not already be in use by another completion queue.
    fn create_cq(
        &mut self,
        cqid: QueueId,
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
        self.cqs[cqid as usize] = Some(Arc::new(cq));
        Ok(())
    }

    /// Creates and stores a new submission queue ([`SubQueue`]) for the controller.
    ///
    /// The specified `sqid` must not already be in use by another submission queue.
    /// The corresponding completion queue specified (`cqid`) must already exist.
    fn create_sq(
        &mut self,
        sqid: QueueId,
        cqid: QueueId,
        base: GuestAddr,
        size: u32,
        ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        if (sqid as usize) >= MAX_NUM_QUEUES {
            return Err(NvmeError::InvalidSubQueue(sqid));
        }
        if self.sqs[sqid as usize].is_some() {
            return Err(NvmeError::SubQueueAlreadyExists(sqid));
        }
        let cq = self.get_cq(cqid)?;
        let sq = SubQueue::new(sqid, cq, size, base, ctx)?;
        self.sqs[sqid as usize] = Some(Arc::new(sq));
        Ok(())
    }

    /// Returns a reference to the [`CompQueue`] which corresponds to the given completion queue id (`cqid`).
    fn get_cq(&self, cqid: QueueId) -> Result<Arc<CompQueue>, NvmeError> {
        if (cqid as usize) >= MAX_NUM_QUEUES {
            return Err(NvmeError::InvalidCompQueue(cqid));
        }
        self.cqs[cqid as usize]
            .as_ref()
            .map(Arc::clone)
            .ok_or(NvmeError::InvalidCompQueue(cqid))
    }

    /// Returns a reference to the [`SubQueue`] which corresponds to the given submission queue id (`cqid`).
    fn get_sq(&self, sqid: QueueId) -> Result<Arc<SubQueue>, NvmeError> {
        if (sqid as usize) >= MAX_NUM_QUEUES {
            return Err(NvmeError::InvalidSubQueue(sqid));
        }
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
    fn get_admin_cq(&self) -> Arc<CompQueue> {
        self.get_cq(queue::ADMIN_QUEUE_ID).unwrap()
    }

    /// Returns a reference to the Admin [`SubQueue`].
    ///
    /// # Panics
    ///
    /// Panics if the Admin Submission Queue hasn't been created yet.
    fn get_admin_sq(&self) -> Arc<SubQueue> {
        self.get_sq(queue::ADMIN_QUEUE_ID).unwrap()
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

    /// Convert some number of logical blocks to bytes with the currently active LBA data size
    fn nlb_to_size(&self, b: usize) -> usize {
        b << (self.ns_ident.lbaf[(self.ns_ident.flbas & 0xF) as usize]).lbads
    }
}

/// NVMe over PCIe
pub struct PciNvme {
    /// NVMe Controller
    state: Mutex<NvmeCtrl>,

    /// Underlying Block Device notifier
    notifier: block::Notifier,

    /// PCI device state
    pci_state: pci::DeviceState,
}

impl PciNvme {
    /// Create a new pci-nvme device with the given values
    pub fn create(
        vendor: u16,
        device: u16,
        binfo: block::DeviceInfo,
    ) -> Arc<Self> {
        let builder = pci::Builder::new(pci::Ident {
            vendor_id: vendor,
            device_id: device,
            sub_vendor_id: vendor,
            sub_device_id: device,
            class: pci::bits::CLASS_STORAGE,
            subclass: pci::bits::SUBCLASS_NVM,
            prog_if: pci::bits::PROGIF_ENTERPRISE_NVME,
            ..Default::default()
        });

        // We have unit tests that these are 16 and 64 bytes, respectively
        // But just make sure as we specify these as powers of 2 in places
        debug_assert!(size_of::<RawCompletion>().is_power_of_two());
        debug_assert!(size_of::<RawSubmission>().is_power_of_two());
        let cqes = size_of::<RawCompletion>().trailing_zeros() as u8;
        let sqes = size_of::<RawSubmission>().trailing_zeros() as u8;

        // Initialize the Identify structure returned when the host issues
        // an Identify Controller command.
        let ctrl_ident = bits::IdentifyController {
            vid: vendor,
            ssvid: vendor,
            // TODO: fill out serial number
            ieee: [0xA8, 0x40, 0x25], // Oxide OUI
            // We use standard Completion/Submission Queue Entry structures with no extra
            // data, so required (minimum) == maximum
            sqes: NvmQueueEntrySize(0).with_maximum(sqes).with_required(sqes),
            cqes: NvmQueueEntrySize(0).with_maximum(cqes).with_required(cqes),
            // Supporting multiple namespaces complicates I/O dispatching,
            // so for now we limit the device to a single namespace.
            nn: 1,
            // bit 0 indicates volatile write cache is present
            vwc: 1,
            ..Default::default()
        };

        // Initialize the Identify structure returned when the  host issues
        // an Identify Namespace command.
        let total_bytes = binfo.total_size * binfo.block_size as u64;
        let nsze = total_bytes / BLOCK_SZ;
        let mut ns_ident = bits::IdentifyNamespace {
            // No thin provisioning so nsze == ncap == nuse
            nsze,
            ncap: nsze,
            nuse: nsze,
            nlbaf: 0, // We only support a single LBA format (1 but 0-based)
            flbas: 0, // And it is at index 0 in the lbaf array
            ..Default::default()
        };

        // Update the block format we support
        debug_assert!(
            BLOCK_SZ.is_power_of_two(),
            "BLOCK_SZ must be a power of 2"
        );
        debug_assert!(BLOCK_SZ >= 512, "BLOCK_SZ must be at least 512 bytes");
        ns_ident.lbaf[0].lbads = BLOCK_SZ.trailing_zeros() as u8;

        // Initialize the CAP "register" leaving most values
        // at their defaults (0):
        //  TO      = 0 => 0ms to wait for controller to be ready
        //  DSTRD   = 0 => 2^(2+0) byte stride for doorbell registers
        //  MPSMIN  = 0 => 2^(12+0) bytes, 4K
        //  MPSMAX  = 0 => 2^(12+0) bytes, 4K
        let cap = Capabilities(0)
            // Allow up to the spec max supported queue size
            // converted to 0's based
            .with_mqes((queue::MAX_QUEUE_SIZE - 1) as u16)
            // I/O Queues must be physically contiguous
            .with_cqr(true)
            // We support the NVM command set
            .with_css_nvm(true);

        // Initialize the CC "register" leaving most values
        // at their defaults (0):
        //  EN      = 0 => controller initially disabled
        //  CSS     = 0 => NVM Command Set selected
        //  MPS     = 0 => 2^(12+0) bytes, 4K pages
        //  AMS     = 0 => Round Robin Arbitration
        //  SHN     = 0 => Shutdown Notification Cleared
        let cc = Configuration(0)
            // Set our expected Submission Queue Entry Size
            .with_iosqes(sqes)
            // Set our expected Completion Queue Entry Size
            .with_iocqes(cqes);

        // Initialize the CSTS "register" leaving most values
        // at their defaults (0):
        //  RDY     = 0 => controller not ready
        //  CFS     = 0 => no fatal controller errors
        //  SHST    = 0 => no shutdown in process, normal operation
        let csts = Status(0);

        let state = NvmeCtrl {
            ctrl: CtrlState { cap, cc, csts, ..Default::default() },
            msix_hdl: None,
            cqs: Default::default(),
            sqs: Default::default(),
            ctrl_ident,
            ns_ident,
            binfo,
        };

        let pci_state = builder
            // XXX: add room for doorbells
            .add_bar_mmio64(pci::BarN::BAR0, CONTROLLER_REG_SZ as u64)
            // BAR0/1 are used for the main config and doorbell registers
            // BAR2 is for the optional index/data registers
            // Place MSIX in BAR4 for now
            .add_cap_msix(pci::BarN::BAR4, NVME_MSIX_COUNT)
            .finish();

        Arc::new(PciNvme {
            state: Mutex::new(state),
            notifier: block::Notifier::new(),
            pci_state,
        })
    }

    /// Service a write to the NVMe Controller Configuration from the VM
    fn ctrlr_cfg_write(
        &self,
        new: Configuration,
        ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        let mut state = self.state.lock().unwrap();
        let cur = state.ctrl.cc;

        if !cur.enabled() {
            // TODO: apply any necessary config changes
        }

        if new.enabled() && !cur.enabled() {
            state.ctrl.cc.set_enabled(true);

            // Create the Admin Completion and Submission queues
            state.create_admin_queues(ctx)?;

            state.ctrl.csts.set_ready(true);
        } else if !new.enabled() && cur.enabled() {
            state.ctrl.cc.set_enabled(false);
            state.ctrl.csts.set_ready(false);

            state.reset();
        }

        let shutdown = new.shn() != ShutdownNotification::None;
        if shutdown && state.ctrl.csts.shst() == ShutdownStatus::Normal {
            // Host has indicated to shutdown
            // TODO: Cleanup properly but for now just immediately indicate
            //       we're done shutting down.
            state.ctrl.csts.set_shst(ShutdownStatus::Complete);
        } else if !shutdown && state.ctrl.csts.shst() != ShutdownStatus::Normal
        {
            state.ctrl.csts.set_shst(ShutdownStatus::Normal);
        }

        Ok(())
    }

    /// Service an NVMe register read from the VM
    fn reg_ctrl_read(
        &self,
        id: &CtrlrReg,
        ro: &mut ReadOp,
        _ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        match id {
            CtrlrReg::CtrlrCaps => {
                let state = self.state.lock().unwrap();
                ro.write_u64(state.ctrl.cap.0);
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
                ro.write_u32(state.ctrl.cc.0);
            }
            CtrlrReg::CtrlrStatus => {
                let state = self.state.lock().unwrap();
                ro.write_u32(state.ctrl.csts.0);
            }
            CtrlrReg::AdminQueueAttr => {
                let state = self.state.lock().unwrap();
                ro.write_u32(state.ctrl.aqa.0);
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
            | CtrlrReg::IOQueueDoorBells => {
                // The host should not read from the doorbells, and the contents
                // can be vendor/implementation specific (in our case, zeroed).
                ro.fill(0);
            }
        }

        Ok(())
    }

    /// Service an NVMe register write from the VM
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
                self.ctrlr_cfg_write(Configuration(wo.read_u32()), ctx)?;
            }
            CtrlrReg::AdminQueueAttr => {
                let mut state = self.state.lock().unwrap();
                if !state.ctrl.cc.enabled() {
                    state.ctrl.aqa = AdminQueueAttrs(wo.read_u32());
                }
            }
            CtrlrReg::AdminSubQAddr => {
                let mut state = self.state.lock().unwrap();
                if !state.ctrl.cc.enabled() {
                    state.ctrl.admin_sq_base = wo.read_u64() & PAGE_MASK as u64;
                }
            }
            CtrlrReg::AdminCompQAddr => {
                let mut state = self.state.lock().unwrap();
                if !state.ctrl.cc.enabled() {
                    state.ctrl.admin_cq_base = wo.read_u64() & PAGE_MASK as u64;
                }
            }

            CtrlrReg::DoorBellAdminSQ => {
                let val = wo.read_u32().try_into().unwrap();
                let state = self.state.lock().unwrap();
                let admin_sq = state.get_admin_sq();
                admin_sq.notify_tail(val)?;

                // Process any new SQ entries
                self.process_admin_queue(state, admin_sq, ctx)?;
            }
            CtrlrReg::DoorBellAdminCQ => {
                let val = wo.read_u32().try_into().unwrap();
                let state = self.state.lock().unwrap();
                let admin_cq = state.get_admin_cq();
                admin_cq.notify_head(val)?;

                // We may have skipped pulling entries off the admin sq
                // due to no available completion entry permit, so just
                // kick it here again in case.
                if admin_cq.kick() {
                    let admin_sq = state.get_admin_sq();
                    self.process_admin_queue(state, admin_sq, ctx)?;
                }
            }

            CtrlrReg::IOQueueDoorBells => {
                // Submission Queue y Tail Doorbell offset
                //  = 0x1000 + (2y * (4 << CAP.DSTRD))
                // Completion Queue y Head Doorbell offset
                //  = 0x1000 + ((2y + 1) * (4 << CAP.DSTRD))
                //
                // See NVMe 1.0e Section 3.1.10 & 3.1.11
                //
                // But note that we only support CAP.DSTRD = 0
                let off = wo.offset() - 0x1000;

                let val: u16 = wo.read_u32().try_into().unwrap();
                let state = self.state.lock().unwrap();

                if (off >> 2) & 0b1 == 0b1 {
                    // Completion Queue y Head Doorbell
                    let y = (off - 4) >> 3;
                    let cq = state.get_cq(y as u16)?;
                    cq.notify_head(val)?;

                    // We may have skipped pulling entries off some SQ due to this
                    // CQ having no available entry slots. Since we've just free'd
                    // up some slots, kick the SQs (excl. admin) here just in case.
                    // TODO: worth kicking only the SQs specifically associated
                    //       with this CQ?
                    if cq.kick() {
                        self.notifier.notify(self, ctx);
                    }
                } else {
                    // Submission Queue y Tail Doorbell
                    let y = off >> 3;
                    let sq = state.get_sq(y as u16)?;
                    sq.notify_tail(val)?;

                    // Poke block device to service new requests
                    self.notifier.notify(self, ctx);
                }
            }
        }

        Ok(())
    }

    /// Process any new entries in the Admin Submission Queue
    fn process_admin_queue(
        &self,
        mut state: MutexGuard<NvmeCtrl>,
        sq: Arc<SubQueue>,
        ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        // Grab the Admin CQ too
        let cq = state.get_admin_cq();

        while let Some((sub, cqe_permit)) = sq.pop(ctx) {
            use cmds::AdminCmd;

            let parsed = AdminCmd::parse(sub);
            if parsed.is_err() {
                // XXX: set controller error state?
                continue;
            }
            let cmd = parsed.unwrap();
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

            cqe_permit.push_completion(sub.cid(), comp, ctx);
        }

        // Notify for any newly added completions
        cq.fire_interrupt(ctx);

        Ok(())
    }
}

impl pci::Device for PciNvme {
    fn bar_rw(&self, bar: pci::BarN, mut rwo: RWOp, ctx: &DispCtx) {
        assert_eq!(bar, pci::BarN::BAR0);
        let f = |id: &CtrlrReg, rwo: RWOp<'_, '_>| {
            let res = match rwo {
                RWOp::Read(ro) => self.reg_ctrl_read(id, ro, ctx),
                RWOp::Write(wo) => self.reg_ctrl_write(id, wo, ctx),
            };
            // TODO: is there a better way to report errors
            if let Err(err) = res {
                eprintln!("nvme reg read/write failed: {}", err);
            }
        };

        if rwo.offset() >= CONTROLLER_REGS.1 {
            // This is an I/O DoorBell op, so skip RegMaps's process
            f(&CtrlrReg::IOQueueDoorBells, rwo);
        } else {
            // Otherwise deal with every other register as normal
            CONTROLLER_REGS.0.process(&mut rwo, f)
        }
    }

    fn attach(&self) {
        // TODO: Update the controller logic to reach out to `pci_state` to get
        // access to the MSIX handle, rather than caching it internally
        let mut state = self.state.lock().unwrap();
        state.msix_hdl = self.pci_state.msix_hdl();
        assert!(state.msix_hdl.is_some());
    }

    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }
}
impl Entity for PciNvme {}

/// NVMe Controller Registers
///
/// See NVMe 1.0e Section 3.1 Register Definition
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum CtrlrReg {
    /// Reserved register.
    Reserved,

    /// Controller Capabilities (CAP)
    ///
    /// See NVMe 1.0e Section 3.1.1 Offset 00h: CAP - Controller Capabilities
    CtrlrCaps,
    /// Version (VS)
    ///
    /// See NVMe 1.0e Section 3.1.2 Offset 08h: VS - Version
    Version,
    /// Interrupt Mask Set (INTMS)
    ///
    /// See NVMe 1.0e Section 3.1.3 Offset 0Ch: INTMS - Interrupt Mask Set
    IntrMaskSet,
    /// Interrupt Mask Clear (INTMC)
    ///
    /// See NVMe 1.0e Section 3.1.4 Offset 10h: INTMC - Interrupt Mask Clear
    IntrMaskClear,
    /// Controller Configuration (CC)
    ///
    /// See NVMe 1.0e Section 3.1.5 Offset 14h: CC - Controller Configuration
    CtrlrCfg,
    /// Controller Status (CSTS)
    ///
    /// See NVMe 1.0e Section 3.1.6 Offset 1Ch: CSTS - Controller Status
    CtrlrStatus,
    /// Admin Queue Attributes (AQA)
    ///
    /// See NVMe 1.0e Section 3.1.7 Offset 24h: AQA - Admin Queue Attributes
    AdminQueueAttr,
    /// Admin Submission Queue Base Address (ASQ)
    ///
    /// See NVMe 1.0e Section 3.1.8 Offset 28h: ASQ - Admin Submission Queue Base Address
    AdminSubQAddr,
    /// Admin Completion Queue Base Address (ACQ)
    ///
    /// See NVMe 1.0e Section 3.1.9 Offset 30h: ACQ - Admin Completion Queue Base Addres
    AdminCompQAddr,

    /// Admin Submission Queue Tail Doorbell
    ///
    /// See NVMe 1.0e Section 3.1.10
    DoorBellAdminSQ,
    /// Admin Completion Queue Head Doorbell
    ///
    /// See NVMe 1.0e Section 3.1.11
    DoorBellAdminCQ,

    /// I/O Submission Tail and Completion Head Doorbells
    ///
    /// See NVMe 1.0e Section 3.1.10 & 3.1.11
    IOQueueDoorBells,
}

/// Size of the Controller Register space
const CONTROLLER_REG_SZ: usize = 0x2000;

lazy_static! {
    static ref CONTROLLER_REGS: (RegMap<CtrlrReg>, usize) = {
        let mut layout = [
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
            // CAP.DSTRD = 0 hence 0 stride and doorbells are 4 bytes apart
            (CtrlrReg::DoorBellAdminSQ, 4),
            (CtrlrReg::DoorBellAdminCQ, 4),
            (CtrlrReg::IOQueueDoorBells, 8 * MAX_NUM_IO_QUEUES),
            // Left as 0 and adjusted below
            (CtrlrReg::Reserved, 0),
        ];

        // Pad out to the next power of two
        let regs_sz = layout.iter().map(|(_, sz)| sz).sum::<usize>();
        assert!(regs_sz.next_power_of_two() <= CONTROLLER_REG_SZ);
        layout.last_mut().unwrap().1 = regs_sz.next_power_of_two() - regs_sz;

        // Find the offset of IOQueueDoorBells
        let db_offset = layout
            .iter()
            .take_while(|&(r,_)| *r != CtrlrReg::IOQueueDoorBells)
            .map(|&(_, sz)| sz)
            .sum();

        (RegMap::create_packed(
            CONTROLLER_REG_SZ,
            &layout,
            Some(CtrlrReg::Reserved),
        ), db_offset)
    };
}
