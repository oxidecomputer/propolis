// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::convert::TryInto;
use std::mem::size_of;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use crate::accessors::Guard;
use crate::block;
use crate::common::*;
use crate::hw::ids::pci::{PROPOLIS_NVME_DEV_ID, VENDOR_OXIDE};
use crate::hw::ids::OXIDE_OUI;
use crate::hw::pci;
use crate::migrate::*;
use crate::util::id::define_id;
use crate::util::regmap::RegMap;
use crate::vmm::MemAccessed;

use futures::future::BoxFuture;
use lazy_static::lazy_static;
use thiserror::Error;

mod admin;
mod bits;
mod cmds;
mod queue;
mod requests;

use bits::*;
use queue::{CompQueue, QueueId, SubQueue};

define_id! {
    /// Identifier for which NVMe controller in the VM an operation is happening
    /// on.
    ///
    /// This is mostly useful for NVMe-related DTrace probes, where otherwise a
    /// queue number or command ID may be ambiguous across distinct NVMe
    /// controllers in a VM.
    #[derive(Copy, Clone)]
    pub struct DeviceId(u32);
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn nvme_doorbell(off: u64, devq_id: u64, is_cq: u8, val: u16) {}
    fn nvme_doorbell_admin_cq(val: u16) {}
    fn nvme_doorbell_admin_sq(val: u16) {}
    fn nvme_admin_cmd(opcode: u8, prp1: u64, prp2: u64) {}
    fn nvme_block_notify(devsq_id: u64, block_devqid: u64, occupied_hint: u16) {
    }
}

/// Combine an NVMe device and queue ID into a single u64 for probes
pub(crate) fn devq_id(dev: DeviceId, queue: QueueId) -> u64 {
    // We'll use the low 16 bits for the queue ID. Assert at compile time that
    // queue IDs cannot go above that. Clippy helpfully asserts this is an
    // absurd comparison, so silence that. If you see a rustc error here you
    // must have changed the type of QueueId such this is no longer absurd!
    #[allow(clippy::absurd_extreme_comparisons)]
    {
        static_assertions::const_assert!(QueueId::MAX <= u16::MAX);
    }

    ((dev.0 as u64) << 16) | (queue as u64)
}

/// The max number of MSI-X interrupts we support
const NVME_MSIX_COUNT: u16 = 1024;

/// NVMe errors
#[derive(Debug, Error)]
pub enum NvmeError {
    /// Unsupported CQES requested
    #[error("the requested CQ entry size is unsupported")]
    UnsupportedCompQueueEntrySize,

    /// Unsupported SQES requested
    #[error("the requested SQ entry size is unsupported")]
    UnsupportedSubQueueEntrySize,

    /// Unsupported AMS requested
    #[error("the requested arbitration mechanism is unsupported")]
    UnsupportedArbitrationMechanism,

    /// Unsupported MPS requested
    #[error("the requested memory page size is unsupported")]
    UnsupportedMemPageSize,

    /// Unsupported CSS requested
    #[error("the requested command set is unsupported")]
    UnsupportedCommandSet,

    /// The specified Completion Queue ID did not correspond to a valid Completion Queue
    #[error("the completion queue specified ({0}) is invalid")]
    InvalidCompQueue(QueueId),

    /// The specified Submission Queue ID did not correspond to a valid Completion Queue
    #[error("the submission queue specified ({0}) is invalid")]
    InvalidSubQueue(QueueId),

    /// The specified Completion Queue ID already exists
    #[error("the completion queue specified ({0}) already exists")]
    CompQueueAlreadyExists(QueueId),

    /// The specified Submission Queue ID already exists
    #[error("the submission queue specified ({0}) already exists")]
    SubQueueAlreadyExists(QueueId),

    /// Can't delete a CQ with associated SQs
    #[error("the completion queue specified ({0}) still has ({1}) associated submission queue(s)")]
    AssociatedSubQueuesStillExist(QueueId, usize),

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

    /// Controller cannot access guest memory
    #[error("memory access inaccessible")]
    MemoryInaccessible,
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
    /// See NVMe 1.0e Section 3.1.8 Offset 28h: ASQ - Admin Submission Queue
    /// Base Address
    admin_sq_base: u64,

    /// The 64-bit Guest address for the Admin Completion Queue
    ///
    /// ACQB
    /// See NVMe 1.0e Section 3.1.9 Offset 30h: ACQ - Admin Completion Queue
    /// Base Address
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
    /// A distinguishing identifier for this NVMe controller across the VM.
    /// Useful mostly to distinguish queues and commands as seen in probes.
    /// `device_id` is held constant across NVMe resets, but not persisted
    /// across export and import.
    device_id: DeviceId,

    /// Internal NVMe Controller state
    ctrl: CtrlState,

    /// Doorbell Buffer Config state
    doorbell_buf: Option<queue::DoorbellBuffer>,

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
}

impl NvmeCtrl {
    /// Creates the admin completion and submission queues.
    ///
    /// Admin queues are always created with `cqid`/`sqid` `0`.
    fn create_admin_queues(&mut self, nvme: &PciNvme) -> Result<(), NvmeError> {
        // Admin CQ uses interrupt vector 0 (See NVMe 1.0e Section 3.1.9 ACQ)
        self.create_cq(
            queue::CreateParams {
                id: queue::ADMIN_QUEUE_ID,
                device_id: self.device_id,
                base: GuestAddr(self.ctrl.admin_cq_base),
                // Convert from 0's based
                size: u32::from(self.ctrl.aqa.acqs()) + 1,
            },
            true,
            0,
            nvme,
        )?;
        self.create_sq(
            queue::CreateParams {
                id: queue::ADMIN_QUEUE_ID,
                device_id: self.device_id,
                base: GuestAddr(self.ctrl.admin_sq_base),
                // Convert from 0's based
                size: u32::from(self.ctrl.aqa.asqs()) + 1,
            },
            true,
            queue::ADMIN_QUEUE_ID,
            nvme,
        )?;
        Ok(())
    }

    /// Creates a new [completion queue](CompQueue) for the controller.
    ///
    /// The CQ ID must not already be in use.  For the admin queue, it must be
    /// 0, while for IO queues, it must _not_ be 0.  This is explicitly enforced
    /// through the `is_admin` argument.
    fn create_cq(
        &mut self,
        params: queue::CreateParams,
        is_admin: bool,
        iv: u16,
        nvme: &PciNvme,
    ) -> Result<Arc<CompQueue>, NvmeError> {
        let cqid = params.id;
        if (cqid as usize) >= MAX_NUM_QUEUES {
            return Err(NvmeError::InvalidCompQueue(cqid));
        }
        if is_admin {
            // Creating admin queue(s) with wrong ID is programmer error
            assert_eq!(cqid, 0);
        } else if cqid == 0 {
            // Guest requests to create an IO queue with the ID belonging to the
            // admin queue is explicitly disallowed.
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
        let cq = Arc::new(CompQueue::new(
            params,
            iv,
            msix_hdl,
            nvme.pci_state.acc_mem.child(Some(format!("CompQueue-{cqid}"))),
        )?);
        if self.doorbell_buf.is_some() {
            cq.set_db_buf(self.doorbell_buf, false);
        }
        self.cqs[cqid as usize] = Some(cq.clone());
        nvme.queues.set_cq_slot(cqid, Some(cq.clone()));
        Ok(cq)
    }

    /// Creates a new [submission queue](SubQueue) for the controller.
    ///
    /// The SQ ID must not already be in use.  For the admin queue, it must be
    /// 0, while for IO queues, it must _not_ be 0.  This is explicitly enforced
    /// through the `is_admin` argument.  The `cqid` to which this SQ will be
    /// associated must correspond to an existing CQ.
    fn create_sq(
        &mut self,
        params: queue::CreateParams,
        is_admin: bool,
        cqid: QueueId,
        nvme: &PciNvme,
    ) -> Result<Arc<SubQueue>, NvmeError> {
        let sqid = params.id;
        if (sqid as usize) >= MAX_NUM_QUEUES {
            return Err(NvmeError::InvalidSubQueue(sqid));
        }
        if is_admin {
            // Creating admin queue(s) with wrong ID is programmer error
            assert_eq!(sqid, 0);
            // So too is associating an admin SQ to an IO CQ
            assert_eq!(cqid, 0);
        } else {
            if sqid == 0 {
                // Guest requests to create an IO queue with the ID belonging to
                // the admin queue is not allowed.
                return Err(NvmeError::InvalidSubQueue(cqid));
            }
            if cqid == 0 {
                // Guest requests to associate the to-be-created IO SQ with an
                // admin CQ is not allowed.
                return Err(NvmeError::InvalidCompQueue(cqid));
            }
        }
        if self.sqs[sqid as usize].is_some() {
            return Err(NvmeError::SubQueueAlreadyExists(sqid));
        }
        let cq = self.get_cq(cqid)?;
        let sq = SubQueue::new(
            params,
            cq,
            nvme.pci_state.acc_mem.child(Some(format!("SubQueue-{sqid}"))),
        )?;
        if self.doorbell_buf.is_some() {
            sq.set_db_buf(self.doorbell_buf, false);
        }
        self.sqs[sqid as usize] = Some(sq.clone());
        nvme.queues.set_sq_slot(sqid, Some(sq.clone()));
        Ok(sq)
    }

    /// Removes the [`CompQueue`] which corresponds to the given completion
    /// queue id (`cqid`).
    fn delete_cq(
        &mut self,
        cqid: QueueId,
        nvme: &PciNvme,
    ) -> Result<(), NvmeError> {
        if (cqid as usize) >= MAX_NUM_QUEUES
            || self.cqs[cqid as usize].is_none()
        {
            return Err(NvmeError::InvalidCompQueue(cqid));
        }

        // Make sure this CQ has no more associated SQs
        let sqs = self.cqs[cqid as usize].as_ref().unwrap().associated_sqs();
        if sqs > 0 {
            return Err(NvmeError::AssociatedSubQueuesStillExist(cqid, sqs));
        }

        // Remove it from the authoritative list of CQs
        self.cqs[cqid as usize] = None;
        nvme.queues.set_cq_slot(cqid, None);
        Ok(())
    }

    /// Removes the [`SubQueue`] which corresponds to the given submission queue id (`sqid`).
    ///
    /// **NOTE:** This only removes the SQ from our list of active SQ and there may still be
    ///           in-flight IO requests for this SQ. But after this call, we'll no longer
    ///           answer any new doorbell requests for this SQ.
    fn delete_sq(
        &mut self,
        sqid: QueueId,
        nvme: &PciNvme,
    ) -> Result<(), NvmeError> {
        if (sqid as usize) >= MAX_NUM_QUEUES
            || self.sqs[sqid as usize].is_none()
        {
            return Err(NvmeError::InvalidSubQueue(sqid));
        }

        // Remove it from the authoritative list of SQs
        self.sqs[sqid as usize] = None;
        nvme.queues.set_sq_slot(sqid, None);
        Ok(())
    }

    /// Returns a reference to the [`CompQueue`] which corresponds to the given completion queue id (`cqid`).
    fn get_cq(&self, cqid: QueueId) -> Result<Arc<CompQueue>, NvmeError> {
        if (cqid as usize) >= MAX_NUM_QUEUES {
            return Err(NvmeError::InvalidCompQueue(cqid));
        }
        self.cqs[cqid as usize].clone().ok_or(NvmeError::InvalidCompQueue(cqid))
    }

    /// Returns a reference to the [`SubQueue`] which corresponds to the given submission queue id (`cqid`).
    fn get_sq(&self, sqid: QueueId) -> Result<Arc<SubQueue>, NvmeError> {
        if (sqid as usize) >= MAX_NUM_QUEUES {
            return Err(NvmeError::InvalidSubQueue(sqid));
        }
        self.sqs[sqid as usize].clone().ok_or(NvmeError::InvalidSubQueue(sqid))
    }

    /// Returns a reference to the Admin [`CompQueue`].
    fn get_admin_cq(&self) -> Result<Arc<CompQueue>, NvmeError> {
        self.get_cq(queue::ADMIN_QUEUE_ID)
    }

    /// Returns a reference to the Admin [`SubQueue`].
    fn get_admin_sq(&self) -> Result<Arc<SubQueue>, NvmeError> {
        self.get_sq(queue::ADMIN_QUEUE_ID)
    }

    /// Perform necessary setup tasks after an IO SQ has been created.
    fn io_sq_post_create(&self, nvme: &PciNvme, sq: Arc<SubQueue>) {
        let sqid = sq.id();
        assert!(sqid != 0, "attempting IO SQ setup on admin SQ");
        sq.update_params(self.transfer_params());
        nvme.block_attach.queue_associate(
            queue::sqid_to_block_qid(sqid),
            requests::NvmeBlockQueue::new(
                sq,
                nvme.pci_state.acc_mem.child(Some(format!("SubQueue-{sqid}"))),
            ),
        );
    }

    /// Configure Controller
    fn configure(&mut self, cc: Configuration) -> Result<(), NvmeError> {
        let mut inner = || {
            // Make sure the requested Queue sizes match our expectations
            // Note: we only compare to `required` as we mandate that
            //       required == maximum. See `Capabilities::mqes` value.
            if cc.iocqes() > 0 {
                if cc.iocqes() != self.ctrl_ident.cqes.required() {
                    return Err(NvmeError::UnsupportedCompQueueEntrySize);
                }
                self.ctrl.cc.set_iocqes(cc.iocqes());
            }
            if cc.iosqes() > 0 {
                if cc.iosqes() != self.ctrl_ident.sqes.required() {
                    return Err(NvmeError::UnsupportedSubQueueEntrySize);
                }
                self.ctrl.cc.set_iosqes(cc.iosqes());
            }

            // These may only be configured while we're disabled
            if !self.ctrl.cc.enabled() {
                // We only support round robin arbitration
                if cc.ams() != ArbitrationMechanism::RoundRobin {
                    return Err(NvmeError::UnsupportedArbitrationMechanism);
                }

                // We only supported an MPS of 0 (4K pages)
                if cc.mps() < self.ctrl.cap.mpsmin()
                    || cc.mps() > self.ctrl.cap.mpsmax()
                {
                    return Err(NvmeError::UnsupportedMemPageSize);
                }

                // No non-standard command sets
                if cc.css() != IOCommandSet::Nvm {
                    return Err(NvmeError::UnsupportedCommandSet);
                }

                self.ctrl.cc.set_ams(cc.ams());
                self.ctrl.cc.set_mps(cc.mps());
                self.ctrl.cc.set_css(cc.css());
            }

            Ok(())
        };

        if let Err(e) = inner() {
            // Got some bad config, set Controller Fail Status
            self.ctrl.csts.set_cfs(true);
            Err(e)
        } else {
            Ok(())
        }
    }

    /// Get the controller in a state ready to process requests
    fn enable(&mut self, nvme: &PciNvme) -> Result<(), NvmeError> {
        // Create the Admin Queues
        self.create_admin_queues(nvme)?;

        Ok(())
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
    fn reset(&mut self, nvme: &PciNvme) {
        // Immediately mark the controller as disabled to prevent any inbound
        // doorbells from being accepted on the queues we are about to delete.
        nvme.is_enabled.store(false, Ordering::Release);

        // Remove our references to the Qs which should be the only strong refs
        // at this point. Any in-flight I/O commands will just implicitly be
        // aborted once they try to issue their completions.
        for (sqid, state_slot) in self.sqs.iter_mut().enumerate() {
            if let Some(_sq) = state_slot.take() {
                let sqid = sqid as QueueId;
                if sqid != queue::ADMIN_QUEUE_ID {
                    // TODO: cancel any existing requests?
                    nvme.block_attach
                        .queue_dissociate(queue::sqid_to_block_qid(sqid));
                }
                nvme.queues.set_sq_slot(sqid, None);
            }
        }
        for (cqid, state_slot) in self.cqs.iter_mut().enumerate() {
            if let Some(_cq) = state_slot.take() {
                nvme.queues.set_cq_slot(cqid as QueueId, None);
            }
        }

        // Clear the CC & CSTS registers
        // Sets CC.EN=0 and CSTS.RDY=0
        self.ctrl.cc = Configuration(0);
        self.ctrl.csts = Status(0);

        // Other bits which are cleared on reset
        self.doorbell_buf = None;

        // The other registers (e.g. CAP/VS) we never modify
        // and thus don't need to do anything on reset
    }

    /// Calculate parameters for Submission Queue data transfer, derived from
    /// the LBA configuration as well as MDTS
    fn transfer_params(&self) -> queue::TransferParams {
        let lba_data_size = 1u64
            << (self.ns_ident.lbaf[(self.ns_ident.flbas & 0xF) as usize]).lbads;
        let max_data_tranfser_size = match self.ctrl_ident.mdts {
            0 => u64::MAX,
            mdts => (self.ctrl.cap.mpsmin_sz() as u64) << mdts,
        };
        queue::TransferParams { lba_data_size, max_data_tranfser_size }
    }

    fn update_block_info(&mut self, info: block::DeviceInfo) {
        let nsze = info.total_size;
        self.ns_ident = bits::IdentifyNamespace {
            // No thin provisioning so nsze == ncap == nuse
            nsze,
            ncap: nsze,
            nuse: nsze,
            ..self.ns_ident
        };
        self.ns_ident.lbaf[0].lbads = info.block_size.trailing_zeros() as u8;

        // Communicate new parameters to SQs
        let params = self.transfer_params();
        self.sqs
            .iter()
            .filter_map(Option::as_ref)
            .for_each(|sq| sq.update_params(params));
    }

    /// Get Memory Page Size (MPS), expressed in bytes
    fn get_mps(&self) -> u64 {
        // "The memory page size is (2 ^ (12 + MPS))"
        1u64 << (12 + self.ctrl.cc.mps())
    }

    fn export(&self) -> migrate::NvmeCtrlV1 {
        let cqs = self.cqs.iter().flatten().map(|cq| cq.export()).collect();
        let sqs = self.sqs.iter().flatten().map(|sq| sq.export()).collect();
        let (dbbuf_shadow, dbbuf_evtidx) = self
            .doorbell_buf
            .map(|buf| (buf.shadow.0, buf.eventidx.0))
            .unwrap_or_else(Default::default);
        migrate::NvmeCtrlV1 {
            cap: self.ctrl.cap.0,
            cc: self.ctrl.cc.0,
            csts: self.ctrl.csts.0,
            aqa: self.ctrl.aqa.0,
            acq_base: self.ctrl.admin_cq_base,
            asq_base: self.ctrl.admin_sq_base,
            dbbuf_shadow,
            dbbuf_evtidx,
            cqs,
            sqs,
        }
    }

    fn import(
        &mut self,
        state: migrate::NvmeCtrlV1,
        nvme: &PciNvme,
    ) -> Result<(), MigrateStateError> {
        // TODO: verify that controller state is consistent with SQ/CQs defined
        // in the payload

        // If any queues exist, clear them out first through a reset.
        self.reset(nvme);

        // TODO: bitstruct doesn't have a validation routine?
        self.ctrl.cap.0 = state.cap;
        self.ctrl.cc.0 = state.cc;
        self.ctrl.csts.0 = state.csts;
        self.ctrl.aqa.0 = state.aqa;

        self.ctrl.admin_cq_base = state.acq_base;
        self.ctrl.admin_sq_base = state.asq_base;

        // Begin with empty DoorbellBuffer state, so it is not automatically
        // configured as we are creating CQs & SQs.
        self.doorbell_buf = None;

        for cqi in state.cqs {
            let is_admin_queue = cqi.id == 0;
            self.create_cq(
                queue::CreateParams {
                    id: cqi.id,
                    device_id: self.device_id,
                    base: GuestAddr(cqi.base),
                    size: cqi.size,
                },
                is_admin_queue,
                cqi.iv,
                nvme,
            )
            .map_err(|e| {
                MigrateStateError::ImportFailed(format!(
                    "NVMe: failed to create CQ: {}",
                    e
                ))
            })?
            .import(cqi)?;
        }

        for sqi in state.sqs {
            let is_admin_queue = sqi.id == 0;
            let sq = self
                .create_sq(
                    queue::CreateParams {
                        id: sqi.id,
                        device_id: self.device_id,
                        base: GuestAddr(sqi.base),
                        size: sqi.size,
                    },
                    is_admin_queue,
                    sqi.cq_id,
                    nvme,
                )
                .map_err(|e| {
                    MigrateStateError::ImportFailed(format!(
                        "NVMe: failed to create SQ: {}",
                        e
                    ))
                })?;
            if !is_admin_queue {
                self.io_sq_post_create(nvme, sq.clone());
            }
            sq.import(sqi)?;
        }

        // With the queues created, we can inject any Doorbell Buffer state.
        //
        // When a guest enables this feature, it results in a write to the
        // buffer page with the current state.  We explicitly skip that step
        // (specifying `is_import = true`) since copying of the guest memory
        // pages will have migrated that state already.
        if state.dbbuf_shadow != 0 && state.dbbuf_evtidx != 0 {
            self.doorbell_buf = Some(queue::DoorbellBuffer {
                shadow: GuestAddr(state.dbbuf_shadow),
                eventidx: GuestAddr(state.dbbuf_evtidx),
            });
            for cq in self.cqs.iter().flatten() {
                cq.set_db_buf(self.doorbell_buf, true);
            }
            for sq in self.sqs.iter().flatten() {
                sq.set_db_buf(self.doorbell_buf, true);
            }
        };

        Ok(())
    }
}

#[derive(Default)]
struct NvmeQueues {
    sqs: [Mutex<Option<Arc<SubQueue>>>; MAX_NUM_QUEUES],
    cqs: [Mutex<Option<Arc<CompQueue>>>; MAX_NUM_QUEUES],
}
impl NvmeQueues {
    /// Replace the contents of a [SubQueue] slot.
    fn set_sq_slot(&self, sqid: QueueId, queue: Option<Arc<SubQueue>>) {
        let replace_some = queue.is_some();

        let old = std::mem::replace(
            &mut *self
                .sqs
                .get(sqid as usize)
                .expect("sqid should be valid")
                .lock()
                .unwrap(),
            queue,
        );

        // We should either be filling an empty slot with a new SQ (during queue
        // creation) or vacating a populated slot (during queue deletion).
        //
        // Swapping an existing SQ for a differing one in a single step would be
        // an unexpected operation.
        if replace_some {
            assert!(old.is_none(), "SQ slot should be empty");
        } else {
            assert!(old.is_some(), "SQ slot should be occupied");
        }
    }

    /// Replace the contents of a [CompQueue] slot.
    fn set_cq_slot(&self, cqid: QueueId, queue: Option<Arc<CompQueue>>) {
        let replace_some = queue.is_some();

        let old = std::mem::replace(
            &mut *self
                .cqs
                .get(cqid as usize)
                .expect("cqid should be valid")
                .lock()
                .unwrap(),
            queue,
        );

        // Same justification in set_sq_slot() above applies to CQs as well
        if replace_some {
            assert!(old.is_none(), "CQ slot should be empty");
        } else {
            assert!(old.is_some(), "CQ slot should be occupied");
        }
    }

    /// Get the slot guard for a given `sqid`, but only if that slot is already
    /// occupied by a [SubQueue].
    ///
    /// (A returned `Some(guard)`  implies `guard.is_some()`)
    fn get_sq(
        &self,
        sqid: QueueId,
    ) -> Option<MutexGuard<'_, Option<Arc<SubQueue>>>> {
        let guard = self.sqs.get(sqid as usize)?.lock().unwrap();
        guard.is_some().then_some(guard)
    }

    /// Get the slot guard for a given `cqid`, but only if that slot is already
    /// occupied by a [CompQueue].
    ///
    /// (A returned `Some(guard)`  implies `guard.is_some()`)
    fn get_cq(
        &self,
        cqid: QueueId,
    ) -> Option<MutexGuard<'_, Option<Arc<CompQueue>>>> {
        let guard = self.cqs.get(cqid as usize)?.lock().unwrap();
        guard.is_some().then_some(guard)
    }
}

/// NVMe over PCIe
pub struct PciNvme {
    /// NVMe Controller
    state: Mutex<NvmeCtrl>,

    /// Duplicate of the controller-enabled (`CC.EN`) state, but not requiring
    /// locking [NvmeCtrl] to read.  It is used to gate per-queue doorbell
    /// accesses without stacking them up behind one central lock.
    is_enabled: AtomicBool,

    /// Duplicate of the controller NVMe device ID, but not requiring locking
    /// [NvmeCtrl] to read.  This is used to provide additional context in
    /// NVMe-related probes.
    device_id: DeviceId,

    /// Access to NVMe Submission and Completion queues.
    ///
    /// These are protected with per-slot (queue ID) locks, so actions taken on
    /// a single queue will not contend with others.  The queue references
    /// contained within are kept in sync with those housed in the [NvmeCtrl]
    /// state.
    queues: NvmeQueues,

    /// PCI device state
    pci_state: pci::DeviceState,

    /// Block attachment point
    pub block_attach: block::DeviceAttachment,

    /// Logger resource
    log: slog::Logger,
}

impl PciNvme {
    /// Create a new pci-nvme device with the given values
    pub fn create(
        serial_number: &[u8; 20],
        mdts: Option<u8>,
        log: slog::Logger,
    ) -> Arc<Self> {
        let builder = pci::Builder::new(pci::Ident {
            vendor_id: VENDOR_OXIDE,
            device_id: PROPOLIS_NVME_DEV_ID,
            sub_vendor_id: VENDOR_OXIDE,
            sub_device_id: PROPOLIS_NVME_DEV_ID,
            class: pci::bits::CLASS_STORAGE,
            subclass: pci::bits::SUBCLASS_STORAGE_NVM,
            prog_if: pci::bits::PROGIF_ENTERPRISE_NVME,
            ..Default::default()
        });

        // We have unit tests that these are 16 and 64 bytes, respectively
        // But just make sure as we specify these as powers of 2 in places
        debug_assert!(size_of::<CompletionQueueEntry>().is_power_of_two());
        debug_assert!(size_of::<SubmissionQueueEntry>().is_power_of_two());
        let cqes = size_of::<CompletionQueueEntry>().trailing_zeros() as u8;
        let sqes = size_of::<SubmissionQueueEntry>().trailing_zeros() as u8;

        // Initialize the Identify structure returned when the host issues
        // an Identify Controller command.
        let ctrl_ident = bits::IdentifyController {
            vid: VENDOR_OXIDE,
            ssvid: VENDOR_OXIDE,
            sn: *serial_number,
            ieee: OXIDE_OUI,
            mdts: mdts.unwrap_or(0),
            // We use standard Completion/Submission Queue Entry structures with no extra
            // data, so required (minimum) == maximum
            sqes: NvmQueueEntrySize(0).with_maximum(sqes).with_required(sqes),
            cqes: NvmQueueEntrySize(0).with_maximum(cqes).with_required(cqes),
            // Supporting multiple namespaces complicates I/O dispatching,
            // so for now we limit the device to a single namespace.
            nn: 1,
            // bit 0 indicates volatile write cache is present
            vwc: 1,
            // bit 8 indicates Doorbell Buffer support
            oacs: (1 << 8),
            ..Default::default()
        };

        // The Identify structure (returned by Identify command issued by guest)
        // will be further updated when a backend is attached to make the
        // underlying device info available.
        let ns_ident = bits::IdentifyNamespace {
            nlbaf: 0, // We only support a single LBA format (1 but 0-based)
            flbas: 0, // And it is at index 0 in the lbaf array
            ..Default::default()
        };

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

        // Initialize the CC "register"
        //  EN      = 0 => controller initially disabled
        //  CSS     = 0 => NVM Command Set selected
        //  MPS     = 0 => 2^(12+0) bytes, 4K pages
        //  AMS     = 0 => Round Robin Arbitration
        //  SHN     = 0 => Shutdown Notification Cleared
        //  IOCQES  = 0 => No I/O CQ Entry Size set yet
        //  IOSQES  = 0 => No I/O SQ Entry Size set yet
        let cc = Configuration(0);

        // Initialize the CSTS "register" leaving most values
        // at their defaults (0):
        //  RDY     = 0 => controller not ready
        //  CFS     = 0 => no fatal controller errors
        //  SHST    = 0 => no shutdown in process, normal operation
        let csts = Status(0);

        let state = NvmeCtrl {
            device_id: DeviceId::new(),
            ctrl: CtrlState { cap, cc, csts, ..Default::default() },
            doorbell_buf: None,
            msix_hdl: None,
            cqs: Default::default(),
            sqs: Default::default(),
            ctrl_ident,
            ns_ident,
        };

        let pci_state = builder
            // BAR0/1 are used for the main config and doorbell registers
            .add_bar_mmio64(pci::BarN::BAR0, CONTROLLER_REG_SZ as u64)
            // BAR2 is for the optional index/data registers
            // Place MSIX in BAR4 for now
            .add_cap_msix(pci::BarN::BAR4, NVME_MSIX_COUNT)
            .finish();

        let block_attach = block::DeviceAttachment::new(
            NonZeroUsize::new(MAX_NUM_IO_QUEUES).unwrap(),
            pci_state.acc_mem.child(Some("block backend".to_string())),
        );

        Arc::new_cyclic(move |self_weak: &Weak<PciNvme>| {
            let this = self_weak.clone();
            block_attach.on_attach(Box::new(move |info| {
                if let Some(this) = Weak::upgrade(&this) {
                    this.state.lock().unwrap().update_block_info(info);
                }
            }));

            // Cache device ID before we move it into the Mutex below.
            let device_id = state.device_id;

            PciNvme {
                state: Mutex::new(state),
                is_enabled: AtomicBool::new(false),
                device_id,
                pci_state,
                queues: NvmeQueues::default(),
                block_attach,
                log,
            }
        })
    }

    /// Service a write to the NVMe Controller Configuration from the VM
    fn ctrlr_cfg_write(&self, new: Configuration) -> Result<(), NvmeError> {
        let mut state = self.state.lock().unwrap();

        // Propogate any CC changes first
        if state.ctrl.cc != new {
            state.configure(new)?;
        }

        let cur = state.ctrl.cc;
        if new.enabled() && !cur.enabled() {
            slog::debug!(self.log, "Enabling controller");

            // Get the controller ready to service requests
            if let Err(e) = state.enable(self) {
                // Couldn't enable controller, set Controller Fail Status
                state.ctrl.csts.set_cfs(true);
                return Err(e);
            } else {
                // Controller now ready to start servicing requests
                // Set CC.EN=1 and CSTS.RDY=1
                state.ctrl.cc.set_enabled(true);
                state.ctrl.csts.set_ready(true);
                self.is_enabled.store(true, Ordering::Release);
            }
        } else if !new.enabled() && cur.enabled() {
            slog::debug!(self.log, "Disabling controller");

            // Reset controller state which will set CC.EN=0 and CSTS.RDY=0
            state.reset(self);
        }

        let shutdown = new.shn() != ShutdownNotification::None;
        if shutdown && state.ctrl.csts.shst() == ShutdownStatus::Normal {
            // Host has indicated to shutdown
            // TODO: Issue flush to underlying block devices
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
                if !state.ctrl.cc.enabled() {
                    ro.write_u32(state.ctrl.aqa.0);
                }
            }
            CtrlrReg::AdminSubQAddr => {
                let state = self.state.lock().unwrap();
                if !state.ctrl.cc.enabled() {
                    ro.write_u64(state.ctrl.admin_sq_base);
                }
            }
            CtrlrReg::AdminCompQAddr => {
                let state = self.state.lock().unwrap();
                if !state.ctrl.cc.enabled() {
                    ro.write_u64(state.ctrl.admin_cq_base);
                }
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
                self.ctrlr_cfg_write(Configuration(wo.read_u32()))?;
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
                // 32-bit register but ignore reserved top 16-bits
                let val = wo.read_u32() as u16;
                probes::nvme_doorbell_admin_sq!(|| val);
                let state = self.state.lock().unwrap();

                if !state.ctrl.cc.enabled() {
                    slog::warn!(
                        self.log,
                        "Doorbell write while controller is disabled"
                    );
                    return Err(NvmeError::InvalidSubQueue(
                        queue::ADMIN_QUEUE_ID,
                    ));
                }

                let admin_sq = state.get_admin_sq()?;
                admin_sq.notify_tail(val)?;

                // Process any new SQ entries
                self.process_admin_queue(state, admin_sq)?;
            }
            CtrlrReg::DoorBellAdminCQ => {
                // 32-bit register but ignore reserved top 16-bits
                let val = wo.read_u32() as u16;
                probes::nvme_doorbell_admin_cq!(|| val);
                let state = self.state.lock().unwrap();

                if !state.ctrl.cc.enabled() {
                    slog::warn!(
                        self.log,
                        "Doorbell write while controller is disabled"
                    );
                    return Err(NvmeError::InvalidCompQueue(
                        queue::ADMIN_QUEUE_ID,
                    ));
                }

                let admin_cq = state.get_admin_cq()?;
                admin_cq.notify_head(val)?;

                // We may have skipped pulling entries off the admin sq
                // due to no available completion entry permit, so just
                // kick it here again in case.
                if admin_cq.kick().is_some() {
                    let admin_sq = state.get_admin_sq()?;
                    self.process_admin_queue(state, admin_sq)?;
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
                //
                // NOTE: Normally the `wo.offset()` would be relative to the
                // beginning of the RegMap-ed register, but writes to the
                // doorbells have a special fast path via PciNvme::bar_write().
                let off = wo.offset() - 0x1000;

                let is_cq = (off >> 2) & 0b1 == 0b1;
                let qid = if is_cq { (off - 4) >> 3 } else { off >> 3 };

                // Queue IDs should be 16-bit and we know
                // `off <= CONTROLLER_REG_SZ (0x4000)`
                let qid = qid.try_into().unwrap();

                // 32-bit register but ignore reserved top 16-bits
                let val = wo.read_u32() as u16;

                // Mix in the device ID for probe purposes
                let devq_id = devq_id(self.device_id, qid);

                probes::nvme_doorbell!(|| (
                    off as u64,
                    devq_id,
                    u8::from(is_cq),
                    val
                ));
                self.ring_doorbell(qid, is_cq, val)?;
            }
        }

        Ok(())
    }

    /// Perform the actual work of a doorbell ring
    fn ring_doorbell(
        &self,
        qid: QueueId,
        is_cq: bool,
        val: u16,
    ) -> Result<(), NvmeError> {
        if !self.is_enabled.load(Ordering::Acquire) {
            slog::debug!(
                self.log,
                "Doorbell write while controller is disabled"
            );
            return Err(if is_cq {
                NvmeError::InvalidCompQueue(qid)
            } else {
                NvmeError::InvalidSubQueue(qid)
            });
        }

        // Note:
        //
        // When notifying SQs as part of a doorbell ring, it is necessary to
        // drop the locks required to access said SQs.
        //
        // Without the protection of those locks, it is possible that racing
        // guest operations to destroy/create IO queues could cause the SQIDs on
        // which we are notifying to be "stale".  This is not a concern, as
        // spurious notifications to the block layer do not have ill effects.

        if is_cq {
            // Completion Queue y Head Doorbell
            let guard = self
                .queues
                .get_cq(qid)
                .ok_or(NvmeError::InvalidCompQueue(qid))?;
            let cq = guard.as_ref().unwrap();

            cq.notify_head(val)?;

            // If this CQ was previously full, SQs may have become corked while
            // trying to get permits.  Notify them that there may now be
            // capacity.
            let Some(to_notify) = cq.kick() else {
                // No associated SQs to notify about
                return Ok(());
            };

            // Querying of SQs (for number of entries available to block layer)
            // and delivery of said notifications must be done with neither CQ
            // or SQ locks held.
            drop(guard);

            for (sqid, num_occupied) in
                to_notify.into_iter().filter_map(|sqid| {
                    assert_ne!(
                        sqid,
                        queue::ADMIN_QUEUE_ID,
                        "IO queues should not associate with Admin queue IDs"
                    );
                    let sq_guard = self.queues.get_sq(sqid)?;
                    let sq = sq_guard.as_ref().unwrap();
                    Some((sqid, sq.num_occupied()))
                })
            {
                let block_qid = queue::sqid_to_block_qid(sqid);
                let devsq_id = devq_id(self.device_id, sqid);
                let block_devqid =
                    block::devq_id(self.block_attach.device_id(), block_qid);
                probes::nvme_block_notify!(|| (
                    devsq_id,
                    block_devqid,
                    num_occupied
                ));
                self.block_attach.notify(
                    block_qid,
                    NonZeroUsize::new(num_occupied as usize),
                );
            }
        } else {
            // Submission Queue y Tail Doorbell
            let guard = self
                .queues
                .get_sq(qid)
                .ok_or(NvmeError::InvalidSubQueue(qid))?;
            let sq = guard.as_ref().unwrap();

            let num_occupied = sq.notify_tail(val)?;
            let devsq_id = sq.devq_id();
            // Notification to block layer cannot be issued with SQ lock held
            drop(guard);

            assert_ne!(qid, queue::ADMIN_QUEUE_ID);
            let block_qid = queue::sqid_to_block_qid(qid);
            let block_devqid =
                block::devq_id(self.block_attach.device_id(), block_qid);
            probes::nvme_block_notify!(|| (
                devsq_id,
                block_devqid,
                num_occupied
            ));
            self.block_attach
                .notify(block_qid, NonZeroUsize::new(num_occupied as usize));
        };
        Ok(())
    }

    /// Process any new entries in the Admin Submission Queue
    fn process_admin_queue(
        &self,
        mut state: MutexGuard<NvmeCtrl>,
        sq: Arc<SubQueue>,
    ) -> Result<(), NvmeError> {
        // Grab the Admin CQ too
        let cq = state.get_admin_cq()?;

        let mem = self.mem_access();
        if mem.is_none() {
            // XXX: set controller error state?
        }
        let mem = mem.unwrap();

        while let Some((sub, permit, _idx)) = sq.pop() {
            use cmds::AdminCmd;

            probes::nvme_admin_cmd!(|| (sub.opcode(), sub.prp1, sub.prp2));
            let cmd = AdminCmd::parse(sub).unwrap_or_else(|_e| {
                // Since unknown admin commands are already parsed into
                // AdminCmd::Unknown, we only need to worry about invalid field
                // contents (such as the fuse bits being set).
                //
                // XXX: set the controller into an error state instead of
                // reacting in the same manner as unknown command?
                AdminCmd::Unknown(sub)
            });
            let comp = match cmd {
                AdminCmd::Abort(cmd) => state.acmd_abort(&cmd),
                AdminCmd::CreateIOCompQ(cmd) => {
                    state.acmd_create_io_cq(&cmd, self)
                }
                AdminCmd::CreateIOSubQ(cmd) => {
                    state.acmd_create_io_sq(&cmd, self)
                }
                AdminCmd::GetLogPage(cmd) => {
                    state.acmd_get_log_page(&cmd, &mem)
                }
                AdminCmd::Identify(cmd) => state.acmd_identify(&cmd, &mem),
                AdminCmd::GetFeatures(cmd) => state.acmd_get_features(&cmd),
                AdminCmd::SetFeatures(cmd) => state.acmd_set_features(&cmd),
                AdminCmd::DeleteIOCompQ(cqid) => {
                    state.acmd_delete_io_cq(cqid, self)
                }
                AdminCmd::DeleteIOSubQ(sqid) => {
                    state.acmd_delete_io_sq(sqid, self)
                }
                AdminCmd::AsyncEventReq => {
                    // async event requests do not appear to be an optional
                    // feature but are not yet supported. The only
                    // command-specific error we could return is "async event
                    // limit exceeded".
                    //
                    // qemu's emulated NVMe also does not support async events
                    // but returns invalid opcode with the do-not-retry flag
                    // set. Do the same so that guest drivers that check for
                    // this can detect it and stop posting async events.
                    cmds::Completion::generic_err(bits::STS_INVAL_OPC).dnr()
                }
                AdminCmd::DoorbellBufCfg(cmd) => {
                    state.acmd_doorbell_buf_cfg(&cmd)
                }
                AdminCmd::Unknown(_) => {
                    cmds::Completion::generic_err(bits::STS_INTERNAL_ERR)
                }
            };

            permit.complete(comp);
        }

        // Notify for any newly added completions
        cq.fire_interrupt();

        Ok(())
    }

    fn mem_access(&self) -> Option<Guard<'_, MemAccessed>> {
        self.pci_state.acc_mem.access()
    }
}

impl pci::Device for PciNvme {
    fn bar_rw(&self, bar: pci::BarN, mut rwo: RWOp) {
        assert_eq!(bar, pci::BarN::BAR0);
        let f = |id: &CtrlrReg, mut rwo: RWOp<'_, '_>| {
            let res = match &mut rwo {
                RWOp::Read(ro) => self.reg_ctrl_read(id, ro),
                RWOp::Write(wo) => self.reg_ctrl_write(id, wo),
            };
            // TODO: is there a better way to report errors
            if let Err(err) = res {
                slog::error!(self.log, "nvme reg r/w failure";
                    "offset" => rwo.offset(),
                    "register" => ?id,
                    "error" => %err
                );
            }
        };

        if rwo.offset() >= CONTROLLER_REGS.db_offset {
            // This is an I/O DoorBell op, so skip RegMaps's process
            f(&CtrlrReg::IOQueueDoorBells, rwo);
        } else {
            // Otherwise deal with every other register as normal
            CONTROLLER_REGS.map.process(&mut rwo, f)
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

impl MigrateMulti for PciNvme {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let ctrl = self.state.lock().unwrap();
        output.push(ctrl.export().into())?;
        drop(ctrl);

        MigrateMulti::export(&self.pci_state, output, ctx)?;

        Ok(())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let input: migrate::NvmeCtrlV1 = offer.take()?;

        let mut ctrl = self.state.lock().unwrap();
        ctrl.import(input, self)?;
        drop(ctrl);

        MigrateMulti::import(&self.pci_state, offer, ctx)?;

        Ok(())
    }
}

impl Lifecycle for PciNvme {
    fn type_name(&self) -> &'static str {
        "pci-nvme"
    }

    fn reset(&self) {
        let mut ctrl = self.state.lock().unwrap();
        ctrl.reset(self);
        self.pci_state.reset(self);
    }

    fn pause(&self) {
        self.block_attach.pause();
    }

    fn resume(&self) {
        self.block_attach.resume();
    }

    fn paused(&self) -> BoxFuture<'static, ()> {
        Box::pin(self.block_attach.none_processing())
    }

    fn migrate(&self) -> Migrator<'_> {
        Migrator::Multi(self)
    }
}

pub mod migrate {
    use crate::migrate::*;

    use serde::{Deserialize, Serialize};

    use super::queue::migrate::{NvmeCompQueueV1, NvmeSubQueueV1};

    #[derive(Deserialize, Serialize)]
    pub struct NvmeCtrlV1 {
        pub cap: u64,
        pub cc: u32,
        pub csts: u32,
        pub aqa: u32,

        pub acq_base: u64,
        pub asq_base: u64,

        pub dbbuf_shadow: u64,
        pub dbbuf_evtidx: u64,

        pub cqs: Vec<NvmeCompQueueV1>,
        pub sqs: Vec<NvmeSubQueueV1>,
    }
    impl Schema<'_> for NvmeCtrlV1 {
        fn id() -> SchemaId {
            ("nvme-ctrl", 1)
        }
    }
}

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
///
/// We specify a size of 0x4000 even though we're not using anywhere near that much
/// space because the NVMe spec requires that bits 13:04 of MLBAR be R/O and 0 on reset.
/// We do that by basically returning a size of 0x4000 which makes us ignore any writes
/// to the bottom 14 bits as needed. See `pci::Bars::reg_write`.
///
/// See NVMe 1.0e Section 2.1.10 Offset 10h: MLBAR (BAR0) - Memory Register Base Address, lower 32 bits
const CONTROLLER_REG_SZ: usize = 0x4000;

struct CtrlRegs {
    map: RegMap<CtrlrReg>,
    db_offset: usize,
}
lazy_static! {
    static ref CONTROLLER_REGS: CtrlRegs = {
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

        // Update the last `Reserved` slot to pad out the rest of the controller register space
        let regs_sz = layout.iter().map(|(_, sz)| sz).sum::<usize>();
        assert!(regs_sz <= CONTROLLER_REG_SZ);
        layout.last_mut().unwrap().1 = CONTROLLER_REG_SZ - regs_sz;

        // Find the offset of IOQueueDoorBells
        let db_offset = layout
            .iter()
            .take_while(|&(r,_)| *r != CtrlrReg::IOQueueDoorBells)
            .map(|&(_, sz)| sz)
            .sum();

        CtrlRegs {
            map: RegMap::create_packed(
                CONTROLLER_REG_SZ,
                &layout,
                Some(CtrlrReg::Reserved),
            ),
            db_offset,
        }
    };
}
