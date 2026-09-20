// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::accessors::MemAccessor;
use crate::block::{self, Backend, BackendOpts, Device, InMemoryBackend};
use crate::hw::pci::{test::Scaffold, Bus, BusLocation, Endpoint};
use crate::migrate::{
    MigrateCtx, MigrateMulti, PayloadOffer, PayloadOffers, PayloadOutputs,
};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::hw::nvme::{
    self,
    bits, AdminQueueAttrs, Configuration, CtrlrReg, GuestAddr, NvmeError,
    PciNvme, SubmissionQueueEntry, CompletionQueueEntry, WriteOp,
};

use crate::vmm::PhysMap;

use crate::lifecycle::Lifecycle;

use rand::{Rng, SeedableRng};
use rand_pcg::Pcg64;
use slog::{Discard, Logger};
use tokio::runtime;

const MB: usize = 1024 * 1024;

/// A test harness and primitive driver for some kinds of fuzz testing of
/// `PciNvme`.
///
/// `PciNvme` (and other drivers) is stateful enough that naive "feed random
/// inputs", or even coverage-guided fuzzing, isn't *incredibly* interesting.
/// Additionally, `PciNvme` is explicitly allowed to be operated on
/// concurrently. A sampling of operations that can reasonably be concurrent:
/// * Reads from or writes to the PCI BAR (maybe concurrent!)
/// * Writes to admin or I/O submission queues (also concurrent!)
/// * Device resets (downstream of VM reboot, typically)
/// * Bonus: VM migration
///
/// `FuzzCtx` intends to bundle state to support and operate `PciNvme`, without
/// imposing on possible tests. Tests will probably find it useful to maintain a
/// state machine where operations on `FuzzCtx` transition between states and
/// allowable successor states. One hope is for `FuzzCtx` and corresponding test
/// state machine to adaptable to `cargo-fuzz`, even with the limitations of
/// coverage-guided fuzzing.
///
/// `FuzzCtx` is too high-level for some tests. Because it manages shared state
/// to drive the emulated device, `FuzzCtx` does not allow arbitrary resets at
/// any point as `PciNvme` technically does. (In practice, reset immediately
/// locks the inner `NvmeCtrl` to do the reset, for administrative options it is
/// effectively serialized anyway.)
struct FuzzCtx {
    // The star of the show, this is the NVMe device that we'll hammer on.
    nvme: Arc<PciNvme>,

    // The rest of the "system" around the emulated device. This is here
    // because we've gotta hold it somewhere, and some parts (`bus`) are
    // swapped out when we replace `nvme` during migrations.
    log: Logger,
    scaffold: Scaffold,
    bus: Bus,

    // This test aims to exercise NVMe emulation and its relationship to
    // backends generally. So use `InMemoryBackend` as it requires the least
    // plumbing to do its part of the job.
    backend: Arc<InMemoryBackend>,

    /// The next CID to use when enqueuing an admin command.
    available_cid: u16,
    /// The next index in the admin submission queue at which an SQE should
    /// be written.
    available_sqe_idx: u16,
}

struct SqState {
    size: u16,
    next_id: u16,
    base_addr: GuestAddr,
    // TODO: phase tag on sqe too? I forget

    avail_ids: Vec<u16>,
    /// All I/Os which have been written to this submission queue and not
    /// yet validated by the test driver yet.
    ///
    /// An I/O may have been written without ringing the submission queue's
    /// doorbell, so the device may not even be aware of it yet. Conversely,
    /// the I/O may have been completed by the device and that completion
    /// even observed by the test driver, without removing the TestIO from
    /// this map.
    ///
    /// I/Os are only "validated" at a WaitIO for that I/O, or at device
    /// reset.
    outstanding_ios: HashMap<u16, TestIO>,
}

impl SqState {
    fn new(base_addr: GuestAddr) -> Self {
        let mut avail_ids = Vec::new();
        // We don't include 0xffff here in deference to NVMe Base
        // Specification (at least 2.0e), which says:
        //
        // > The value of FFFFh should not be used as the Error Information
        // > log page (refer to section 5.16.1.2) uses this value to
        // > indicate an error is not associated with a particular command.
        for i in 0..=0xfffe {
            avail_ids.push(i);
        }
        Self {
            size: FuzzCtx::IO_QUEUE_ENTRIES,
            next_id: 0,
            base_addr,
            avail_ids,
            outstanding_ios: HashMap::new()
        }
    }

    fn write_sqe(&mut self, sqe: SubmissionQueueEntry, acc_mem: &MemAccessor) {
        let sqe_size = std::mem::size_of::<SubmissionQueueEntry>();
        let next_addr = GuestAddr(self.base_addr.0 + self.next_id as u64 * sqe_size as u64);
        eprintln!("writing sqe to sq slot {}, addr {:x}", self.next_id, next_addr.0);

        acc_mem.access().unwrap().write(
            next_addr,
            &sqe,
        );

        self.next_id += 1;
        if self.next_id == self.size {
            self.next_id = 0;
        }
    }

    fn curr_idx(&self) -> u16 {
        if self.next_id == 0 {
            0xffff
        } else {
            self.next_id - 1
        }
    }

    fn full(&self) -> bool {
        self.outstanding_ios.len() as u16 == self.size
    }

    fn empty(&self) -> bool {
        self.outstanding_ios.len() as u16 == 0
    }

    fn acquire_cid(&mut self) -> Option<u16> {
        self.avail_ids.pop()
    }
    fn outstanding_cid(&self) -> Option<u16> {
        self.outstanding_ios.keys().next().copied()
    }
    fn release_cid(&mut self, id: u16) {
        self.outstanding_ios.remove(&id);
    }
}

struct CqState {
    base_addr: GuestAddr,
    size: u16,
    /// The status of the Phase Tag to be seen in new completions written to
    /// this queue.
    phase: bool,
    /// The last index we saw a completion on this completion queue.
    next_id: u16,
}

impl CqState {
    fn new(base_addr: GuestAddr) -> Self {
        Self {
            base_addr,
            size: FuzzCtx::IO_QUEUE_ENTRIES,
            // > When ..  an I/O Completion Queue for the first time after
            // > the Create I/O Completion Queue command completed for that
            // > queue, the Phase Tag bit for that completion queue entry is
            // > set to 1
            phase: true,
            next_id: 0,
        }
    }

    fn poll_cqe(&mut self, acc_mem: &MemAccessor) -> Option<CompletionQueueEntry> {
        let cqe_size = std::mem::size_of::<CompletionQueueEntry>();
        let next_addr = GuestAddr(self.base_addr.0 + self.next_id as u64 * cqe_size as u64);
        eprintln!("reading cqe from cq slot {}, addr {:x}", self.next_id, next_addr.0);

        let cqe = acc_mem.access().unwrap().read::<CompletionQueueEntry>(
            next_addr,
        ).expect("can read cqe address");

        let cqe_phase = cqe.status_phase & 1 == 1;
        eprintln!("got cqe: {:?}", *cqe);
        if cqe_phase != self.phase {
            return None;
        }

        self.next_id += 1;
        if self.next_id == self.size {
            self.next_id = 0;
            self.phase = !self.phase;
        }

        Some(*cqe)
    }
}

struct TestIO {
    /// The operation which resulted in this I/O
    op: TestOperation,
    /// The test device's corresponding CQE, to compare against the
    /// requested operation and device state.
    ///
    /// If this is `None`, the test driver hasn't seen a completion from the
    /// device yet. If this is `Some`, the test driver saw a completion and
    /// stashed it here, but a specific WaitIO for this TestIO hasn't been
    /// seen yet.
    completion: Option<CompletionQueueEntry>,
}

impl FuzzCtx {
    /// Arbitrary 20-byte serial.
    const TEST_SERIAL: &'static [u8; 20] = b"11112222333344445555";
    // Bus location doesn't matter all that much, we'll happen to poke the
    // NVMe device via `ctrl_reg_write` directly anyway.
    const TEST_NVME_LOCATION: BusLocation = BusLocation::new(0, 0).unwrap();

    const SQE_SIZE: usize = std::mem::size_of::<SubmissionQueueEntry>();

    const ADMIN_SQ_ENTRIES: u16 = 1024;
    const ADMIN_SQ_BASE: GuestAddr = GuestAddr(MB as u64);
    const ADMIN_SQ_SIZE: usize =
        Self::ADMIN_SQ_ENTRIES as usize * Self::SQE_SIZE;

    const ADMIN_CQ_ENTRIES: u16 = 1024;
    const ADMIN_CQ_BASE: GuestAddr =
        GuestAddr(Self::ADMIN_SQ_BASE.0 + Self::ADMIN_SQ_SIZE as u64);

    // Place I/O queues arbitrarily at the end of memory.
    const IO_QUEUES_BASE: usize = 2 * MB - (256 * 1024);
    // We won't do much with the queues, so they don't need to be deep.
    const IO_QUEUE_ENTRIES: u16 = 64;
    // And SQEs are larger than CQEs, so we'll just use the larger size for
    // all I/O queues.
    const IO_QUEUE_SIZE: usize =
        Self::SQE_SIZE * (Self::IO_QUEUE_ENTRIES as usize);

    const IO_MEM_BASE: usize = 1 * MB;
    const IO_MEM_END: usize = 1 * MB + 512 * 1024;

    fn new(log: &Logger) -> Self {
        let mut scaffold = Scaffold::new();

        // Scaffold sets up an orphan acc_mem. Swap it with a more-real
        // memory mapping, which we'll use for admin queue operations later.
        let mut map = PhysMap::new_test(2 * MB);
        // Test RAM starts at 1 MB and is 1 MB large.
        map.add_test_mem("test-ram".to_string(), MB, MB)
            .expect("can create test memory region");
        scaffold.acc_mem = map.finalize();

        let bus = scaffold.create_bus();

        // 64 MB feels like a reasonable (but very tiny!) size for a test
        // disk.
        //
        // TODO: actually perform reads/writes against the test disk. At
        // that point it probably makes sense to have more than one worker
        // as well.
        let backend = InMemoryBackend::create(
            vec![0; 64 * MB],
            BackendOpts {
                block_size: Some(512),
                read_only: Some(false),
                skip_flush: Some(false),
            },
            NonZeroUsize::new(1).unwrap(),
        )
        .unwrap();

        let nvme = PciNvme::create(Self::TEST_SERIAL, None, true, log.clone());

        block::attach(
            nvme.attachment(),
            backend.attachment(),
        )
        .unwrap();
        bus.attach(
            Self::TEST_NVME_LOCATION,
            Arc::clone(&nvme) as Arc<dyn Endpoint>,
            None,
        );

        nvme.start().unwrap();
        use tokio::runtime;
        let rt = runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(&mut (backend.clone() as Arc<dyn Backend>).start()).unwrap();

        Self {
            nvme,

            log: log.clone(),
            scaffold,
            bus,

            backend,

            available_cid: 0,
            available_sqe_idx: 0,
        }
    }

    // I/O submission/completion queues are interleaved (for fun more than
    // anything else). With 256kb of memory for queues we can have
    // up to 64 I/O queues in the form of 32 submission and completion
    // queues.
    fn io_sq_address(i: u16) -> GuestAddr {
        assert!(i < 32, "invalid I/O submission queue id");
        GuestAddr(
            (Self::IO_QUEUES_BASE + i as usize * 2 * Self::IO_QUEUE_SIZE)
                as u64,
        )
    }

    fn io_cq_address(i: u16) -> GuestAddr {
        assert!(i < 32, "invalid I/O completion queue id");
        GuestAddr(
            (Self::IO_QUEUES_BASE + (i as usize * 2 + 1) * Self::IO_QUEUE_SIZE)
                as u64,
        )
    }

    // TODO: this is a wildly insufficient means for picking command IDs.
    // This probably should be a list of available IDs with IDs picked off
    // the front and returned when the operation completes.
    fn next_cid(&mut self) -> u16 {
        let result = self.available_cid;

        self.available_cid = self.available_cid + 1;

        // Wrap here; the highest value we should give out is 0xfffe.
        //
        // In the section `Submission Queue Entry`, the NVMe base
        // specification suggests not using CID=FFFFh as that is the value
        // used by the Error Information log page to indicate that an error
        // is not associated with a particular command.
        if self.available_cid == 0xffff {
            self.available_cid = 0;
        }

        result
    }

    // TODO: also wildly insufficient queue management. This assumes there
    // is exactly one admin queue operation in flight at a time, so we'll
    // never run over the tail of the admin submission queue.
    fn next_sqe_idx(&mut self) -> u16 {
        let result = self.available_sqe_idx;

        self.available_sqe_idx = self.available_sqe_idx.wrapping_add(1);

        result
    }

    // Do the steps to initialize the controller and set it running.
    fn init_controller(&mut self) -> Result<(), NvmeError> {
        let aqa = AdminQueueAttrs(0)
            .with_asqs(Self::ADMIN_SQ_ENTRIES)
            .with_acqs(Self::ADMIN_CQ_ENTRIES)
            .0
            .to_le_bytes();

        self.nvme.reg_ctrl_write(
            &CtrlrReg::AdminQueueAttr,
            &mut WriteOp::from_buf(0, &aqa),
        )?;

        // `Machine::new_test` puts RAM at 1MB..2MB, so we'll put the submission queue at
        // 1MB and the completion queue at 1Mib + 64KiB
        self.nvme.reg_ctrl_write(
            &CtrlrReg::AdminSubQAddr,
            &mut WriteOp::from_buf(0, &Self::ADMIN_SQ_BASE.0.to_le_bytes()),
        )?;

        self.nvme.reg_ctrl_write(
            &CtrlrReg::AdminCompQAddr,
            &mut WriteOp::from_buf(0, &Self::ADMIN_CQ_BASE.0.to_le_bytes()),
        )?;

        let cfg = Configuration(0)
            .with_enabled(true)
            .with_iosqes(6)
            .with_iocqes(4)
            .0
            .to_le_bytes();

        self.nvme.reg_ctrl_write(
            &CtrlrReg::CtrlrCfg,
            &mut WriteOp::from_buf(0, &cfg),
        )?;

        use crate::hw::nvme::ReadOp;
        use crate::hw::nvme::CtrlrReg;
        let mut buf = [0; 4];
        let mut read_op = ReadOp::from_buf(0, &mut buf);
        self.nvme.reg_ctrl_read(&CtrlrReg::CtrlrStatus, &mut read_op)?;
        eprintln!("{:x?}", buf);
        assert!(buf[0] & 2 == 0);

        self.available_cid = 0;
        self.available_sqe_idx = 0;

        Ok(())
    }

    /// Write the provided `SubmissionQueueEntry` into the NVMe device's
    /// admin SQ and ring the doorbell to force the SQE's evaluation.
    fn drive_admin_sqe(
        &mut self,
        mut sqe: SubmissionQueueEntry,
    ) -> Result<(), NvmeError> {
        let cid = self.next_cid();
        let sq_idx = self.next_sqe_idx();

        sqe.cdw0 |= (cid as u32) << 16;

        self.scaffold.acc_mem.access().unwrap().write(
            Self::ADMIN_SQ_BASE + Self::SQE_SIZE * (sq_idx as usize),
            &sqe,
        );

        // TODO: most accurately we should wait for a corresponding admin CQ
        // entry with phase tag set. We might even wait to check that until
        // an interrupt is fired. In practice, `reg_ctrl_write` evalues the
        // admin command synchronously, so returning is sufficient to know
        // processing is done.
        let res = self.nvme.reg_ctrl_write(
            &CtrlrReg::DoorBellAdminSQ,
            &mut WriteOp::from_buf(0, &(sq_idx as u32 + 1).to_le_bytes()),
        );

        res
    }

    fn create_cq(&mut self, cqid: u16) -> Result<(), NvmeError> {
        let create_completion_queue = SubmissionQueueEntry {
            cdw0: bits::ADMIN_OPC_CREATE_IO_CQ as u32,
            cdw10: (Self::IO_QUEUE_ENTRIES as u32 - 1) << 16 | cqid as u32,
            // IV 2, interrupts enabled, is physically contiguous
            cdw11: 0x0002_0003,
            prp1: Self::io_cq_address(cqid).0,
            ..Default::default()
        };

        self.drive_admin_sqe(create_completion_queue)
    }

    fn create_sq(&mut self, sqid: u16) -> Result<(), NvmeError> {
        let create_submission_queue = SubmissionQueueEntry {
            cdw0: bits::ADMIN_OPC_CREATE_IO_SQ as u32,
            cdw10: (Self::IO_QUEUE_ENTRIES as u32 - 1) << 16 | sqid as u32,
            // completions go to same-ID CQ, is physically contiguous
            cdw11: ((sqid as u32) << 16) | 0x0001,
            prp1: Self::io_sq_address(sqid).0,
            ..Default::default()
        };

        self.drive_admin_sqe(create_submission_queue)
    }

    fn delete_sq(&mut self, sqid: u16) -> Result<(), NvmeError> {
        let delete_submission_queue = SubmissionQueueEntry {
            cdw0: bits::ADMIN_OPC_DELETE_IO_SQ as u32,
            cdw10: sqid as u32,
            ..Default::default()
        };

        self.drive_admin_sqe(delete_submission_queue)
    }

    fn delete_cq(&mut self, cqid: u16) -> Result<(), NvmeError> {
        let delete_submission_queue = SubmissionQueueEntry {
            cdw0: bits::ADMIN_OPC_DELETE_IO_CQ as u32,
            cdw10: cqid as u32,
            ..Default::default()
        };

        self.drive_admin_sqe(delete_submission_queue)
    }

    fn doorbell(&mut self, qid: u16, sq_idx: u16) -> Result<(), NvmeError> {
        let doorbell_addr = 0x1000 + ((qid as usize) << 3);
        eprintln!("doorbell! to {:x}, val={}", qid, sq_idx);
        let res = self.nvme.reg_ctrl_write(
            &CtrlrReg::IOQueueDoorBells,
            &mut WriteOp::from_buf(doorbell_addr, &(sq_idx as u32 + 1).to_le_bytes()),
        );
        res
    }

    fn submit_read(&mut self, sq: &mut SqState, lba: u64, memptr: usize, len: u64, cid: u16) -> Result<(), NvmeError> {
        let lba_lo = lba as u32;
        let lba_hi = (lba >> 32) as u32;
        let nlb = len / 4096;
        let cdw0 = (bits::NVM_OPC_READ as u32) | ((cid as u32) << 16);
        let sqe = SubmissionQueueEntry {
            cdw0,
            cdw10: lba_lo,
            cdw11: lba_hi,
            cdw12: nlb as u32 - 1,
            prp1: memptr as u64,
            prp2: 0,
            ..Default::default()
        };

        sq.write_sqe(sqe, &self.scaffold.acc_mem);

        Ok(())
    }

    fn submit_write(&mut self, sq: &mut SqState, lba: u64, memptr: usize, len: u64, cid: u16) -> Result<(), NvmeError> {
        let lba_lo = lba as u32;
        let lba_hi = (lba >> 32) as u32;
        let nlb = len / 4096;
        let cdw0 = (bits::NVM_OPC_WRITE as u32) | ((cid as u32) << 16);
        let sqe = SubmissionQueueEntry {
            cdw0,
            cdw10: lba_lo,
            cdw11: lba_hi,
            cdw12: nlb as u32 - 1,
            prp1: memptr as u64,
            prp2: 0,
            ..Default::default()
        };

        sq.write_sqe(sqe, &self.scaffold.acc_mem);

        Ok(())
    }

    fn poll_cq(&mut self, cq: &mut CqState) -> Result<Vec<CompletionQueueEntry>, NvmeError> {
        let mut cqes = Vec::new();
        if let Some(cqe) = cq.poll_cqe(&self.scaffold.acc_mem) {
            cqes.push(cqe)
        }
        Ok(cqes)
    }

    /// Reset this fuzzing context to the start of the state machine: a
    /// fresh device and at the start of the fuzzing state machine.
    fn reset(&mut self) {
        self.nvme.reset();
        self.available_cid = 0;
        self.available_sqe_idx = 0;
    }

    /// Migrate the emulated NVMe device.
    ///
    /// Concurrent operations on the device are blocked during "migration".
    /// It is not possible to operate on a device whose state has been
    /// exported, nor a fresh replacement device before state has been
    /// imported. This is consistent with practical uses of devices, where
    /// vCPUs are stopped while migrating out.
    fn nvme_migrate(&mut self) {
        self.nvme.pause();
        let rt = runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(&mut (self.backend.clone() as Arc<dyn Backend>).stop());

        let mut payload_outputs = PayloadOutputs::new();
        let acc_mem = self.scaffold.acc_mem.access().unwrap();
        let migrate_ctx = MigrateCtx { mem: &acc_mem };

        self.nvme
            .export(&mut payload_outputs, &migrate_ctx)
            .expect("can export");

        let mut data = Vec::new();
        let payload_outputs = payload_outputs.into_iter().collect::<Vec<_>>();
        let mut desers: Vec<ron::Deserializer> =
            Vec::with_capacity(payload_outputs.len());
        let mut metadata: Vec<(&str, u32)> =
            Vec::with_capacity(payload_outputs.len());

        let mut payload_offers = {
            for payload in payload_outputs.iter() {
                data.push(
                    ron::ser::to_string(&payload.payload)
                        .expect("can serialize"),
                );
            }
            for (payload, data) in payload_outputs.iter().zip(data.iter()) {
                desers.push(
                    ron::Deserializer::from_str(data).expect("can deserialize"),
                );
                metadata.push((&payload.kind, payload.version));
            }
            let offer_iter =
                metadata.iter().zip(desers.iter_mut()).map(|(meta, deser)| {
                    PayloadOffer {
                        kind: meta.0,
                        version: meta.1,
                        payload: Box::new(
                            <dyn erased_serde::Deserializer>::erase(deser),
                        ),
                    }
                });
            PayloadOffers::new(offer_iter)
        };

        self.nvme = PciNvme::create(Self::TEST_SERIAL, None, true, self.log.clone());

        // TODO: we don't have a way to detach the exported NVMe device from
        // the bus, so we'll replace the whole bus and attach the new NVMe
        // device to the new bus.
        self.bus = self.scaffold.create_bus();

        self.backend.attachment().detach();
        block::attach(
            self.nvme.attachment(),
            self.backend.attachment(),
        )
        .unwrap();

        self.bus.attach(
            Self::TEST_NVME_LOCATION,
            Arc::clone(&self.nvme) as Arc<dyn Endpoint>,
            None,
        );

        self.nvme
            .import(&mut payload_offers, &migrate_ctx)
            .expect("can import");

        self.nvme.start().unwrap();
        let rt = runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(&mut (self.backend.clone() as Arc<dyn Backend>).start()).unwrap();
    }
}

// Probably-insufficient enum describing what we should expect for a given
// operation.
//
// It might be nice to include enough information to assert on specifics about
// what was ok, or what kind of error occurred. This is expected to be part of
// whatever function processes `TestAction`, below.
#[derive(Copy, Clone, Debug, PartialEq)]
enum Expected {
    Ok,
    Err,
}

impl Expected {
    fn check(&self, res: &Result<(), NvmeError>) {
        match (self, res) {
            (Expected::Ok, Ok(_)) | (Expected::Err, Err(_)) => {
                // As expected, we can continue on.
            }
            (_, Ok(_)) => {
                // This is rather unfortunate. If an admin command completed
                // with an error in `process_admin_queue`, the error itself
                // isn't propagated outward. We may have gotten an `Ok(())` even
                // if the command was not successfully processed; we can't tell
                // if it was the expected result or not.
            }
            (expected, actual) => {
                panic!(
                    "Unexpected result: got {:?}, wanted {:?}",
                    actual, expected
                );
            }
        }
    }
}

impl TestAction {
    fn ok(op: TestOperation) -> Self {
        TestAction { op, result: Expected::Ok }
    }

    // TODO: might be nice to test for specific kinds of error? Since
    // different operations might error in different ways, this probably
    // isn't sufficient for that kind of detail.
    fn err(op: TestOperation) -> Self {
        TestAction { op, result: Expected::Err }
    }
}

#[derive(Copy, Clone, Debug)]
enum TestOperation {
    Reset,
    Migrate,
    Init,
    CreateSQ(u16),
    CreateCQ(u16),
    DeleteSQ(u16),
    DeleteCQ(u16),
    Doorbell(u16),
    SubmitRead { queue: u16, lba: u64, memptr: usize, size: u64, fresh_cid: bool },
    SubmitWrite { queue: u16, lba: u64, memptr: usize, size: u64, fresh_cid: bool },
    WaitIO { queue: u16, cid: u16 },
}

#[derive(Copy, Clone, Debug)]
struct TestAction {
    op: TestOperation,
    result: Expected,
}

#[test]
fn fuzzy() -> Result<(), NvmeError> {
    let log = Logger::root(Discard, slog::o!());

    let mut fuzz_ctx = FuzzCtx::new(&log);

    /// Track expected device state so we take mostly-legal actions (and can
    /// tell when we take illegal actions)
    ///
    /// This explicit split of "pick test actions" -> "execute test actions"
    /// might be over-built. Hopefully makes it a little easier to plug into
    /// cargo-fuzz and debug interesting execution.
    struct TestState {
        initialized: bool,
        /// Submission and completion queue arrays are 17 entries, but only 16
        /// should ever be used. Queue ID 0 is for admin queues, and is fully
        /// unused here.
        submission_queues: Vec<Option<SqState>>,
        completion_queues: Vec<Option<CqState>>,
        /// The maximum number of queues the device supports (matching the size
        /// of the Vecs, above)
        max_queues: usize,
        /// The number of bytes in the device's first namespace. This corresponds
        /// to `IdentifyNamespace`'s `NUSE` times the namespace's LBA size.
        ns_size: u64,
    }

    impl TestState {
        fn new() -> Self {
            let mut this = Self {
                initialized: false,
                submission_queues: Vec::new(),
                completion_queues: Vec::new(),
                // TODO: This should be read from the device under test, but
                // just using the constant will do for now.
                max_queues: 2, //nvme::MAX_NUM_QUEUES,
                // TODO: Should read this from `IdentifyNamespace`, but the test
                // backend is made right up there and it's a fixed size..
                ns_size: 64 * MB as u64,
            };

            // TODO: as with `max_queues` above, this should be read from the
            // device under test, but it's all constants today.
            this.submission_queues.resize_with(this.max_queues, || None);
            this.completion_queues.resize_with(this.max_queues, || None);

            this
        }

        fn apply(&mut self, fuzz_ctx: &mut FuzzCtx, action: TestAction) {
            match action.op {
                TestOperation::Init => {
                    eprintln!("doing init!");
                    let res = fuzz_ctx.init_controller();

                    action.result.check(&res);

                    if action.result == Expected::Ok {
                        self.initialized = true;
                    }
                }
                TestOperation::Migrate => {
                    fuzz_ctx.nvme_migrate();
                }
                TestOperation::Reset => {
                    fuzz_ctx.reset();
                    eprintln!("TestOperation::Reset");
                    *self = TestState::new();
                }
                TestOperation::CreateCQ(qid) => {
                    let res = fuzz_ctx.create_cq(qid);

                    action.result.check(&res);

                    if action.result == Expected::Ok {
                        let cq_addr = FuzzCtx::io_cq_address(qid);

                        self.completion_queues[qid as usize] = Some(CqState::new(cq_addr));
                    }
                }
                TestOperation::CreateSQ(qid) => {
                    let res = fuzz_ctx.create_sq(qid);

                    action.result.check(&res);

                    if action.result == Expected::Ok {
                        let sq_addr = FuzzCtx::io_sq_address(qid);

                        self.submission_queues[qid as usize] = Some(SqState::new(sq_addr));
                    }
                }
                TestOperation::DeleteCQ(qid) => {
                    let res = fuzz_ctx.delete_cq(qid);

                    action.result.check(&res);

                    if action.result == Expected::Ok {
                        self.completion_queues[qid as usize] = None;
                    }
                }
                TestOperation::DeleteSQ(qid) => {
                    let res = fuzz_ctx.delete_sq(qid);

                    action.result.check(&res);

                    if action.result == Expected::Ok {
                        self.submission_queues[qid as usize] = None;
                    }
                }
                TestOperation::Doorbell(qid) => {
                    let sq = self.submission_queues[qid as usize].as_ref()
                        .expect("only ringing doorbell on queues that exist");

                    assert!(self.initialized);

                    let res = fuzz_ctx.doorbell(qid, sq.curr_idx());

                    action.result.check(&res);

                    if action.result == Expected::Ok {
                        self.submission_queues[qid as usize] = None;
                    }
                }
                TestOperation::SubmitRead { queue, lba, memptr, size, fresh_cid } => {
                    let sq = self.submission_queues[queue as usize].as_mut()
                        .expect("sq exists when we submit writes");
                    let command_id = if fresh_cid {
                        sq.acquire_cid()
                            .expect("planner made sure a CID is available to acquire")
                    } else {
                        sq.outstanding_cid()
                            .expect("planner made sure a CID is present to reuse")
                    };
                    eprintln!("submitting read {} q={}, mem={:x} lba={}", queue, command_id, memptr, lba);
                    let res = fuzz_ctx.submit_read(sq, lba, memptr, size, command_id);

                    sq.outstanding_ios.insert(command_id, TestIO { op: action.op, completion: None });

                    action.result.check(&res);

                    if action.result != Expected::Ok {
                        let sq = self.submission_queues[queue as usize].as_mut()
                            .expect("sqid exists to release unused cid");
                        sq.release_cid(command_id);
                    }
                }
                TestOperation::SubmitWrite { queue, lba, memptr, size, fresh_cid } => {
                    let sq = self.submission_queues[queue as usize].as_mut()
                        .expect("sq exists when we submit writes");
                    let command_id = if fresh_cid {
                        sq.acquire_cid()
                            .expect("planner made sure a CID is available to acquire")
                    } else {
                        sq.outstanding_cid()
                            .expect("planner made sure a CID is present to reuse")
                    };
                    eprintln!("submitting write {} q={}, mem={:x} lba={}", queue, command_id, memptr, lba);
                    let res = fuzz_ctx.submit_write(sq, lba, memptr, size, command_id);

                    sq.outstanding_ios.insert(command_id, TestIO { op: action.op, completion: None });

                    action.result.check(&res);

                    if action.result != Expected::Ok {
                        let sq = self.submission_queues[queue as usize].as_mut()
                            .expect("sqid exists to release unused cid");
                        sq.release_cid(command_id);
                    }
                }
                TestOperation::WaitIO { queue, cid } => {
                    let sq = self.submission_queues[queue as usize].as_mut()
                        .expect("WaitIO only issued for I/O queues that are fully established");

                    // It is a fuzz harness error for a WaitIO to be issued for
                    // qid/cid that is not actually in flight. The I/O may have
                    // been completed, though, in which case there is a
                    // completion which we're about to process.
                    assert!(sq.outstanding_ios.contains_key(&cid));

                    fuzz_ctx.doorbell(queue, sq.curr_idx())
                        .expect("doorbell");

                    let deadline = SystemTime::now().checked_add(Duration::from_secs(1))
                        .expect("time can go forward");

                    loop {
                        let sq = self.submission_queues[queue as usize].as_mut()
                            .expect("WaitIO only issued for I/O queues that are fully established");

                        if let Some(completion) = sq.outstanding_ios[&cid].completion.as_ref() {
                            // TODO: verify the I/O completion somehow?
                            action.result.check(&Ok(()));
                            eprintln!("DONE with I/O {} on queue {}", cid, queue);
                            sq.outstanding_ios.remove(&cid);
                            break;
                        }

                        std::thread::sleep(Duration::from_millis(10));

                        if SystemTime::now() > deadline {
                            panic!("i/o never happened");
                        }

                        let cq = self.completion_queues[queue as usize].as_mut()
                            .expect("WaitIO only issued for I/O queues that are fully established");

                        for completion in fuzz_ctx.poll_cq(cq).expect("can poll cq") {
                            eprintln!("new completion: {:?}", completion);
                            self.handle_completion(completion);
                        }
                    }
                }
            }
        }

        fn handle_completion(&mut self, completion: CompletionQueueEntry) {
            let sq = self.submission_queues[completion.sqid as usize]
                .as_mut().expect("completion implies there is an sq");
            let cid = completion.cid;
            let io = sq.outstanding_ios.get_mut(&cid)
                .expect("there is a submission for the completion");
            let prior_completion = io.completion.replace(completion);

            // If we've seen a completion for an I/O, we .. should not have seen
            // that I/O be completed before! We won't submit a new SQE with this
            // CID until we've WaitIO'd on the existing one.
            assert!(prior_completion.is_none());
        }

        fn options(&self, rng: &mut impl Rng) -> Vec<TestAction> {
            use TestOperation::*;

            // A migration is always allowed, and should never change device
            // state.
            let mut res = vec![TestAction::ok(Migrate)];

            if rng.random_ratio(1, 100) {
                // Reset is always allowed, at the cost of device state. Give
                // this a relatively low chance so we have an opportunity to
                // explore more interesting paths.
                res.push(TestAction::ok(Reset));
            }

            // Pick an operation on one I/O queue pair because the 16 * 4
            // options across the whole slate deflates the odds we pick a valid
            // action.
            let qpid = rng.random_range(1..self.max_queues as u16);

            if !self.initialized {
                // If we haven't initialized the controller yet, we can either
                // do that or see operations on queues all fail.
                res.push(TestAction::ok(Init));
                res.push(TestAction::err(CreateSQ(qpid)));
                res.push(TestAction::err(CreateCQ(qpid)));
                res.push(TestAction::err(DeleteSQ(qpid)));
                res.push(TestAction::err(DeleteCQ(qpid)));

                return res;
            }

            match (
                self.completion_queues[qpid as usize].as_ref(),
                self.submission_queues[qpid as usize].as_ref(),
            ) {
                (None, None) => {
                    // Neither CQ nor SQ is created yet. Creating an SQ here
                    // will fail (for using an invalid CQ), creating a CQ
                    // here should succeed.
                    res.push(TestAction::ok(CreateCQ(qpid)));
                    res.push(TestAction::err(CreateSQ(qpid)));
                    res.push(TestAction::err(DeleteCQ(qpid)));
                    res.push(TestAction::err(DeleteSQ(qpid)));
                }
                (Some(_cq), None) => {
                    res.push(TestAction::err(CreateCQ(qpid)));
                    res.push(TestAction::ok(CreateSQ(qpid)));
                    res.push(TestAction::ok(DeleteCQ(qpid)));
                    res.push(TestAction::err(DeleteSQ(qpid)));
                }
                (Some(_cq), Some(sq)) => {
                    if rng.random_ratio(2, 100) {
                        res.push(TestAction::err(CreateCQ(qpid)));
                        res.push(TestAction::err(CreateSQ(qpid)));
                        res.push(TestAction::err(DeleteCQ(qpid)));
                        res.push(TestAction::ok(DeleteSQ(qpid)));
                    }

                    if rng.random_ratio(90, 100) {
                        let lba = rng.random_range(0..self.ns_size) / 4096;
                        let io_addr = FuzzCtx::IO_MEM_BASE + rng.random_range(0..128usize) * 4096;

                        if !sq.full() {
                            // TODO: different I/O sizes
                            res.push(TestAction::ok(SubmitRead {
                                queue: qpid,
                                lba,
                                memptr: io_addr,
                                size: 4096,
                                fresh_cid: true,
                            }));
                            res.push(TestAction::ok(SubmitWrite {
                                queue: qpid,
                                lba,
                                memptr: io_addr,
                                size: 4096,
                                fresh_cid: true,
                            }));
                        }
                        if let Some(pending_cid) = sq.outstanding_cid() {
                            res.push(TestAction::ok(WaitIO { queue: qpid, cid: pending_cid }));
                        }
                    }

                    if !sq.empty() {
                        res.push(TestAction::ok(Doorbell(qpid)));
                    }
                }
                (None, Some(_sq)) => {
                    panic!("sq {} exists but cq does not?", qpid);
                }
            }

            res
        }
    }

    let mut test_state = TestState::new();

    let seed = rand::random::<u64>();
    eprintln!("fuzzing nvme from seed {:#016x}", seed);

    let mut rng = Pcg64::seed_from_u64(seed);

    for _ in 0..1_000 {
        let options = test_state.options(&mut rng);
        let next = options[rng.random_range(0..options.len())];
//        eprintln!("operation: {:?}", next);
        test_state.apply(&mut fuzz_ctx, next);
    }

    Ok(())
}
