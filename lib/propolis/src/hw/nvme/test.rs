// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::accessors::MemAccessor;
use crate::block::{self, Backend, BackendOpts, InMemoryBackend};
use crate::hw::pci::{test::Scaffold, Bus, BusLocation, Endpoint};
use crate::migrate::{
    MigrateCtx, MigrateMulti, PayloadOffer, PayloadOffers, PayloadOutputs,
};
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::hw::nvme::{
    bits, AdminQueueAttrs, Configuration, CtrlrReg, GuestAddr, NvmeError,
    PciNvme, SubmissionQueueEntry, WriteOp,
};

use crate::vmm::PhysMap;

use crate::lifecycle::Lifecycle;

use rand::{Rng, SeedableRng};
use rand_pcg::Pcg64;
use slog::{Discard, Logger};

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

impl FuzzCtx {
    /// Arbitrary 20-byte serial.
    const TEST_SERIAL: &'static [u8; 20] = b"11112222333344445555";
    // Bus location doesn't matter all that much, we'll happen to poke the
    // NVMe device via `ctrl_reg_write` directly anyway.
    const TEST_NVME_LOCATION: BusLocation = BusLocation::new(0, 0).unwrap();

    const SQE_SIZE: usize = std::mem::size_of::<SubmissionQueueEntry>();

    const ADMIN_SQ_ENTRIES: u16 = 1024;
    const ADMIN_SQ_BASE: GuestAddr = GuestAddr(1 * MB as u64);
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

    fn new(log: &Logger) -> Self {
        let mut scaffold = Scaffold::new();

        // Scaffold sets up an orphan acc_mem. Swap it with a more-real
        // memory mapping, which we'll use for admin queue operations later.
        let mut map = PhysMap::new_test(2 * MB);
        // Test RAM starts at 1 MB and is 1 MB large.
        map.add_test_mem("test-ram".to_string(), MB, MB)
            .expect("can create test memory region");
        scaffold.acc_mem = MemAccessor::new(map.memctx());

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

        let nvme = PciNvme::create(Self::TEST_SERIAL, None, log.clone());

        block::attach(
            Arc::clone(&nvme) as Arc<dyn block::Device>,
            Arc::clone(&backend) as Arc<dyn Backend>,
        )
        .unwrap();
        bus.attach(
            Self::TEST_NVME_LOCATION,
            Arc::clone(&nvme) as Arc<dyn Endpoint>,
            None,
        );

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

        self.nvme = PciNvme::create(Self::TEST_SERIAL, None, self.log.clone());

        // TODO: we don't have a way to detach the exported NVMe device from
        // the bus, so we'll replace the whole bus and attach the new NVMe
        // device to the new bus.
        self.bus = self.scaffold.create_bus();

        self.backend.attachment().detach().unwrap();
        block::attach(
            Arc::clone(&self.nvme) as Arc<dyn block::Device>,
            Arc::clone(&self.backend) as Arc<dyn Backend>,
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
        // Submission and completion queue arrays are 17 entries, but only 16
        // should ever be used. Queue ID 0 is for admin queues, and is fully
        // unused here.
        submission_queues: [bool; 17],
        completion_queues: [bool; 17],
    }

    impl TestState {
        fn new() -> Self {
            Self {
                initialized: false,
                submission_queues: [false; 17],
                completion_queues: [false; 17],
            }
        }

        fn apply(&mut self, fuzz_ctx: &mut FuzzCtx, action: TestAction) {
            match action.op {
                TestOperation::Init => {
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
                    *self = TestState::new();
                }
                TestOperation::CreateCQ(qid) => {
                    let res = fuzz_ctx.create_cq(qid);

                    action.result.check(&res);

                    if action.result == Expected::Ok {
                        self.completion_queues[qid as usize] = true;
                    }
                }
                TestOperation::CreateSQ(qid) => {
                    let res = fuzz_ctx.create_sq(qid);

                    action.result.check(&res);

                    if action.result == Expected::Ok {
                        self.submission_queues[qid as usize] = true;
                    }
                }
                TestOperation::DeleteCQ(qid) => {
                    let res = fuzz_ctx.delete_cq(qid);

                    action.result.check(&res);

                    if action.result == Expected::Ok {
                        self.completion_queues[qid as usize] = false;
                    }
                }
                TestOperation::DeleteSQ(qid) => {
                    let res = fuzz_ctx.delete_sq(qid);

                    action.result.check(&res);

                    if action.result == Expected::Ok {
                        self.submission_queues[qid as usize] = false;
                    }
                }
            }
        }

        fn options(&self, rng: &mut impl Rng) -> Vec<TestAction> {
            use TestOperation::*;

            // A migration is always allowed, and should never change device
            // state.
            let mut res = vec![TestAction::ok(Migrate)];

            if rng.random_ratio(1, 10) {
                // Reset is always allowed, at the cost of device state. Give
                // this a relatively low chance so we have an opportunity to
                // explore more interesting paths.
                res.push(TestAction::ok(Reset));
            }

            // Pick an operation on one I/O queue pair because the 16 * 4
            // options across the whole slate deflates the odds we pick a valid
            // action.
            let qpid = rng.random_range(1..17);

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
                self.completion_queues[qpid as usize],
                self.submission_queues[qpid as usize],
            ) {
                (false, false) => {
                    // Neither CQ nor SQ is created yet. Creating an SQ here
                    // will fail (for using an invalid CQ), creating a CQ
                    // here should succeed.
                    res.push(TestAction::ok(CreateCQ(qpid)));
                    res.push(TestAction::err(CreateSQ(qpid)));
                    res.push(TestAction::err(DeleteCQ(qpid)));
                    res.push(TestAction::err(DeleteSQ(qpid)));
                }
                (true, false) => {
                    res.push(TestAction::err(CreateCQ(qpid)));
                    res.push(TestAction::ok(CreateSQ(qpid)));
                    res.push(TestAction::ok(DeleteCQ(qpid)));
                    res.push(TestAction::err(DeleteSQ(qpid)));
                }
                (true, true) => {
                    res.push(TestAction::err(CreateCQ(qpid)));
                    res.push(TestAction::err(CreateSQ(qpid)));
                    res.push(TestAction::err(DeleteCQ(qpid)));
                    res.push(TestAction::ok(DeleteSQ(qpid)));
                }
                (false, true) => {
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
        test_state.apply(&mut fuzz_ctx, next);
    }

    Ok(())
}
