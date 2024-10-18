// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use bitvec::prelude::{BitSlice, Lsb0};
use futures::{SinkExt, StreamExt};
use propolis::common::{GuestAddr, GuestData, PAGE_SIZE};
use propolis::migrate::{
    MigrateCtx, MigrateStateError, Migrator, PayloadOutputs,
};
use propolis::vmm;
use propolis_api_types::instance_spec::VersionedInstanceSpec;
use slog::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::convert::TryInto;
use std::io;
use std::ops::{Range, RangeInclusive};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{tungstenite, WebSocketStream};
use uuid::Uuid;

use crate::migrate::codec::Message;
use crate::migrate::memx;
use crate::migrate::preamble::Preamble;
use crate::migrate::probes;
use crate::migrate::protocol::Protocol;
use crate::migrate::{codec, protocol};
use crate::migrate::{
    Device, DevicePayload, MigrateError, MigratePhase, MigrateRole,
    MigrationState, PageIter,
};

use crate::vm::objects::VmObjects;
use crate::vm::state_publisher::{
    ExternalStateUpdate, MigrationStateUpdate, StatePublisher,
};

use super::MigrateConn;

/// Specifies which pages should be offered during a RAM transfer phase.
///
/// On Dirty Pages and the Discipline Thereof
/// -----------------------------------------
///
/// In an ideal world, a migration would only ever transfer pages of the guest's
/// address space which have actually been touched by the guest; we don't want
/// to waste time sending a bunch of zero pages on the wire. RAM is offered to
/// the migration destination in two phases: first, we transfer the majority of
/// the guest's RAM prior to pausing the guest, and second, after pausing the
/// guest, we transfer any pages which have been touched since when we performed
/// the first transfer. This way, we perform most of the RAM transfer while the
/// guest is still running, and only re-transfer pages which have been dirtied
/// again while paused.
///
/// Transferring only dirty pages is made possible by bhyve's
/// `VM_TRACK_DIRTY_PAGES` and `VM_NPT_OPERATION`` ioctls. The
/// `VM_TRACK_DIRTY_PAGES` ioctl allows us to generate a bitmap of which pages
/// have their dirty flags set, allowing us to offer only those pages when
/// performing a RAM transfer. Because `VM_TRACK_DIRTY_PAGES`` also clears the
/// dirty bits, if we call the ioctl when offering the initial pre-pause RAM
/// transfer, and then again when performing the post-pause RAM transfer, the
/// second ioctl call will see only the dirty bits that were set *since* when
/// the initial transfer was performed, allowing us to offer only the pages
/// which have been touched since we transferred most of the memory.
///
/// Sounds simple enough, right? Well, here's where things get interesting.
/// Should a migration *fail* after transferring RAM, all the dirty bits on the
/// guest's pages will have been cleared by the `VM_TRACK_DIRTY_PAGES` ioctl.
/// This means that, for a naive implementation which just always transfers only
/// the pages marked as dirty by the ioctl, a second or third migration attempt
/// will not transfer any pages whose dirty bits were cleared by the first
/// migration attempt and haven't been touched again since then. This is bad
/// news! The guest has written to those pages, and, just because it hasn't
/// touched them since the last migration attempt, it may still care about that
/// memory, and attempt to read what it put there again in the future --- with
/// unpleasant results, if we haven't transferred that memory.
///
/// There are two potential ways we can solve this, and --- as you're about to
/// discover --- we implement both of them:
///
/// 1. The obvious solution: we can just offer all RAM in the pre-pause RAM push
///    phase, clearing any dirty bits, and then offer only dirty pages in the
///    post-pause RAM push phase. This has the nice property that it's trivially
///    correct no matter how many migration attempts it takes before we actually
///    migrate a guest successfully. It also has the less nice property that
///    we're offering a bunch of pages that the guest has never actually touched
///    and doesn't care about.
/// 2. The clever solution: what if we had a way to put the dirty bit *back*
///    after we've cleared it? If such a thing existed, we could record which
///    pages were dirty when we performed the RAM transfers, and then, should
///    the migration fail, go back and put those dirty bits *back*, so that a
///    potential future migration attempt will still see that those pages are
///    dirty and offer them again.
///
/// The good news is that there is, in fact, a way to do that, using the
/// `VM_NPT_OPERATION` ioctl's `VNO_OP_SET_DIRTY` operation (which, in Propolis,
/// we pronounce like [`VmmHdl::set_dirty_pages`]). However, the less good news
/// is that this ioctl isn't available in all the bhyve versions that Propolis
/// supports, as it was added in bhyve v17. Therefore, we implement both
/// solutions, depending on which bhyve version is present. If we have
/// VM_NPT_OPERATION, both the pre-pause and post-pause RAM push phases will use
/// [`RamOfferDiscipline::OfferDirty`], and only offer dirty pages, recording
/// any dirty bits that were cleared by the `VM_TRACK_DIRTY_PAGES` ioctl. If we
/// don't have `VM_NPT_OPERATION`, we use [`RamOfferDiscipline::OfferAll`] in
/// the pre-pause phase, and [`RamOfferDiscipline::OfferDirty`] only in the
/// post-pause phase. Because we can't put the dirty bits back, we have to offer
/// all the memory in the first phase, but we can still use dirty page tracking
/// to avoid re-offering pages that were transferred in the first phase when we
/// do the second RAM offer after pausing the guest.
#[derive(Debug)]
enum RamOfferDiscipline {
    /// Offer all pages irrespective of whether they are dirty.
    OfferAll,

    /// Offer only pages that the hypervisor are marked as dirty.
    OfferDirty,
}

/// The interface to an arbitrary version of the source half of the live
/// migration protocol.
//
// Use `async_trait` here to help generate a `Send` bound on the futures
// returned by the functions in this trait.
#[async_trait::async_trait]
pub(crate) trait SourceProtocol {
    /// Runs live migration out of the supplied `vm_objects`, writing back any
    /// state that must be saved for future migration attempts to
    /// `persistent_state`.
    ///
    /// This routine guarantees that the supplied `vm_objects` are paused on
    /// success and resumed on failure.
    async fn run(
        self,
        vm_objects: &VmObjects,
        publisher: &mut StatePublisher,
        persistent_state: &mut PersistentState,
    ) -> Result<(), MigrateError>;
}

/// Negotiates a live migration protocol version with a target who has connected
/// over `conn`. If this is successful, returns a `SourceProtocol`
/// implementation that can be used to run the requested migration.
pub(crate) async fn initiate<T: MigrateConn>(
    log: &slog::Logger,
    migration_id: Uuid,
    mut conn: WebSocketStream<T>,
    vm_objects: &VmObjects,
    persistent_state: &PersistentState,
) -> Result<impl SourceProtocol, MigrateError> {
    // Create a new log context for the migration
    let log = log.new(slog::o!(
        "migration_id" => migration_id.to_string(),
        "migrate_role" => "source"
    ));
    info!(log, "negotiating migration as source");

    // The protocol should start with some text from the destination identifying
    // the protocol versions it supports.
    let dst_protocols = match conn.next().await {
        Some(Ok(tungstenite::Message::Text(dst_protocols))) => dst_protocols,
        x => {
            error!(
                log,
                "destination side did not begin migration version handshake: \
                 {:?}",
                x
            );

            // Tell the destination it misbehaved. This is best-effort.
            if let Err(e) = conn
                .send(tungstenite::Message::Close(Some(CloseFrame {
                    code: CloseCode::Protocol,
                    reason: "did not begin with version handshake.".into(),
                })))
                .await
            {
                warn!(log, "failed to send handshake failed message to source";
                      "error" => ?e);
            }

            return Err(MigrateError::Initiate);
        }
    };

    // Pick the most favorable protocol from the list the destination supplied
    // and send it back to the destination.
    info!(log, "destination offered protocols: {}", dst_protocols);
    let selected = match protocol::select_protocol_from_offer(&dst_protocols) {
        Ok(Some(selected)) => {
            conn.send(tungstenite::Message::Text(selected.offer_string()))
                .await?;
            selected
        }
        Ok(None) => {
            let src_protocols = protocol::make_protocol_offer();
            error!(
                log,
                "no compatible destination protocols";
                "dst_protocols" => &dst_protocols,
                "src_protocols" => &src_protocols,
            );
            return Err(MigrateError::NoMatchingProtocol(
                src_protocols,
                dst_protocols,
            ));
        }
        Err(e) => {
            error!(log, "failed to parse destination protocol offer";
                           "dst_protocols" => &dst_protocols,
                           "error" => %e);
            return Err(MigrateError::ProtocolParse(
                dst_protocols,
                e.to_string(),
            ));
        }
    };

    info!(log, "selected protocol {:?}", selected);
    match selected {
        Protocol::RonV0 => Ok(RonV0::new(
            log,
            vm_objects,
            migration_id,
            conn,
            persistent_state,
        )
        .await),
    }
}

/// State which must be stored across multiple migration attempts.
///
/// This struct is stored on the [`VmController`] so that may be accessed by
/// subsequent [`migrate`] invocations.

#[derive(Default)]
pub(crate) struct PersistentState {
    /// Set if we were unable to re-set dirty bits on guest pages after a failed
    /// migration attempt. If this occurs, we can no longer offer only dirty
    /// pages in a subsequent migration attempt, as some pages which should be
    /// marked as dirty may not be.
    pub(crate) has_redirtying_ever_failed: bool,
}

/// Context for the source side of protocol version 0 using the RON encoding.
struct RonV0<T: MigrateConn> {
    /// The logger to which to log messages from this migration attempt.
    log: slog::Logger,

    /// The migration's ID.
    migration_id: Uuid,

    /// Transport to the destination Instance.
    conn: WebSocketStream<T>,

    /// Guest page table dirty bits to restore in the event of a migration
    /// failure, so that a subsequent migration can attempt to offer only dirty
    /// pages. These dirty bits are accumulated across all RAM push phases, so
    /// that any pages which become dirty after the pre-pause RAM push are also
    /// added to these bitmaps.
    ///
    /// This is `Some` if we should attempt to re-set dirty bits on guest pages
    /// in the event of a migration failure, and `None` if we cannot do so. It
    /// will be `Some` if (and only if):
    ///
    /// - The current bhyve version supports the `VM_NPT_OPERATION` ioctl (i.e.
    ///   it is at least bhyve v17 or later),
    /// - A previous attempt to re-dirty pages has failed (viz.
    ///   [`PersistentState::has_retrying_ever_failed`]), in which case, we can
    ///   no longer trust  that all previously dirtied pages were re-dirtied
    ///   correctly.
    ///
    /// Otherwise, we must fall back to always offering all pages in the initial
    /// pre-pause RAM push phase.
    dirt: Option<HashMap<GuestAddr, PageBitmap>>,
}

const PAGE_BITMAP_SIZE: usize = 4096;
type PageBitmap = [u8; PAGE_BITMAP_SIZE];

impl<T: MigrateConn> RonV0<T> {
    async fn new(
        log: slog::Logger,
        vm: &VmObjects,
        migration_id: Uuid,
        conn: WebSocketStream<T>,
        persistent_state: &PersistentState,
    ) -> Self {
        // Create a (prospective) dirty page map if bhyve supports the NPT
        // API. If this map is present and the VM hasn't recorded that it's
        // possibly unhealthy, it will be used to offer only dirty pages during
        // the pre-pause RAM push.
        let dirt = {
            let can_npt_operate =
                vm.lock_shared().await.vmm_hdl().can_npt_operate();

            let has_redirtying_ever_failed =
                persistent_state.has_redirtying_ever_failed;
            if can_npt_operate && !has_redirtying_ever_failed {
                Some(Default::default())
            } else {
                info!(
                    log,
                    "guest pages not redirtyable, will offer all pages in pre-pause";
                    "can_npt_operate" => can_npt_operate,
                    "has_redirtying_ever_failed" => has_redirtying_ever_failed
                );
                None
            }
        };
        Self { log, migration_id, conn, dirt }
    }
}

#[async_trait::async_trait]
impl<T: MigrateConn> SourceProtocol for RonV0<T> {
    async fn run(
        self,
        vm_objects: &VmObjects,
        publisher: &mut StatePublisher,
        persistent_state: &mut PersistentState,
    ) -> Result<(), MigrateError> {
        let mut runner = RonV0Runner {
            log: self.log,
            migration_id: self.migration_id,
            conn: self.conn,
            dirt: self.dirt,
            vm: vm_objects,
            state_publisher: publisher,
            persistent_state,
            paused: false,
        };

        runner.run().await
    }
}

struct RonV0Runner<'vm, T: MigrateConn> {
    log: slog::Logger,
    migration_id: Uuid,
    conn: WebSocketStream<T>,
    dirt: Option<HashMap<GuestAddr, PageBitmap>>,
    vm: &'vm VmObjects,
    state_publisher: &'vm mut StatePublisher,
    persistent_state: &'vm mut PersistentState,
    paused: bool,
}

impl<'vm, T: MigrateConn> RonV0Runner<'vm, T> {
    fn log(&self) -> &slog::Logger {
        &self.log
    }

    fn update_state(&mut self, state: MigrationState) {
        self.state_publisher.update(ExternalStateUpdate::Migration(
            MigrationStateUpdate {
                state,
                id: self.migration_id,
                role: MigrateRole::Source,
            },
        ));
    }

    async fn pause_vm(&mut self) {
        assert!(!self.paused);
        self.paused = true;
        self.vm.lock_exclusive().await.pause().await;
    }

    async fn resume_vm(&mut self) {
        assert!(self.paused);
        self.paused = false;
        self.vm.lock_exclusive().await.resume();
    }

    async fn run_phase(
        &mut self,
        step: MigratePhase,
    ) -> Result<(), MigrateError> {
        probes::migrate_phase_begin!(|| { step.to_string() });

        let res = match step {
            MigratePhase::MigrateSync => self.sync().await,
            MigratePhase::Pause => self.pause().await,
            MigratePhase::RamPushPrePause | MigratePhase::RamPushPostPause => {
                self.ram_push(&step).await
            }
            MigratePhase::TimeData => self.time_data().await,
            MigratePhase::DeviceState => self.device_state().await,
            MigratePhase::RamPull => self.ram_pull().await,
            MigratePhase::ServerState => self.server_state().await,
            MigratePhase::Finish => self.finish().await,
        };

        probes::migrate_phase_end!(|| { step.to_string() });

        res
    }

    async fn run(&mut self) -> Result<(), MigrateError> {
        info!(self.log(), "Entering Source Migration Task");

        let result: Result<_, MigrateError> = async {
            self.run_phase(MigratePhase::MigrateSync).await?;
            self.run_phase(MigratePhase::RamPushPrePause).await?;
            self.run_phase(MigratePhase::Pause).await?;
            self.run_phase(MigratePhase::RamPushPostPause).await?;
            self.run_phase(MigratePhase::TimeData).await?;
            self.run_phase(MigratePhase::DeviceState).await?;
            self.run_phase(MigratePhase::RamPull).await?;
            self.run_phase(MigratePhase::ServerState).await?;
            self.run_phase(MigratePhase::Finish).await?;
            Ok(())
        }
        .await;

        if let Err(err) = result {
            self.update_state(MigrationState::Error);
            let _ = self.send_msg(codec::Message::Error(err.clone())).await;

            // If we are capable of setting the dirty bit on guest page table
            // entries, re-dirty them, so that a later migration attempt can also
            // only offer dirty pages. If we can't use VM_NPT_OPERATION, a
            // subsequent migration attempt will offer all pages.
            //
            // See the lengthy comment on `RamOfferDiscipline` above for more
            // details about what's going on here.
            let vmm_hdl = self.vm.lock_shared().await.vmm_hdl().clone();
            for (&GuestAddr(gpa), dirtiness) in self.dirt.iter().flatten() {
                if let Err(e) = vmm_hdl.set_dirty_pages(gpa, dirtiness) {
                    // Bad news! Our attempt to re-set the dirty bit on these
                    // pages has failed! Thus, subsequent migration attempts
                    // /!\ CAN NO LONGER RELY ON DIRTY PAGE TRACKING /!\
                    // and must always offer all pages in the initial RAM push
                    // phase.
                    //
                    // Record that now so we never try to do this again.
                    self.persistent_state.has_redirtying_ever_failed = true;
                    error!(
                        self.log(),
                        "failed to restore dirty bits: {e}";
                        "gpa" => gpa,
                    );
                    // No sense continuing to try putting back any remaining
                    // dirty bits, as we won't be using them any longer.
                    break;
                } else {
                    debug!(self.log(), "re-dirtied pages at {gpa:#x}",);
                }
            }

            if self.paused {
                self.resume_vm().await;
            }

            Err(err)
        } else {
            // The VM should be paused after successfully migrating out; the
            // state driver assumes as much when subsequently halting the
            // instance.
            assert!(self.paused);
            info!(self.log(), "Source Migration Successful");
            Ok(())
        }
    }

    async fn sync(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Sync);
        let preamble = Preamble::new(VersionedInstanceSpec::V0(
            self.vm.lock_shared().await.instance_spec().clone().into(),
        ));
        let s = ron::ser::to_string(&preamble)
            .map_err(codec::ProtocolError::from)?;
        self.send_msg(codec::Message::Serialized(s)).await?;

        self.read_ok().await
    }

    async fn ram_push(
        &mut self,
        phase: &MigratePhase,
    ) -> Result<(), MigrateError> {
        match phase {
            MigratePhase::RamPushPrePause => {
                self.update_state(MigrationState::RamPush)
            }
            MigratePhase::RamPushPostPause => {
                self.update_state(MigrationState::RamPushDirty)
            }
            _ => unreachable!("should only push RAM in a RAM push phase"),
        }

        let vmm_ram_range = self.vmm_ram_bounds().await?;
        let req_ram_range = self.read_mem_query().await?;
        info!(
            self.log(),
            "ram_push ({:?}): got query for range {:#x?}, vm range {:#x?}",
            phase,
            req_ram_range,
            vmm_ram_range
        );

        // Determine whether we can offer only dirty pages, or if we must offer
        // all pages.
        //
        // Refer to the giant comment on `RamOfferDiscipline` above for more
        // details about this determination.
        let offer_discipline = match phase {
            // If we are in the pre-pause RAM push phase, and we don't have
            // VM_NPT_OPERATION to put back any dirty bits if the migration
            // fails, we have to offer all pages here.
            MigratePhase::RamPushPrePause if self.dirt.is_none() => {
                RamOfferDiscipline::OfferAll
            }
            // Otherwise, if we are in the post-pause phase, or if we *can* just
            // put back the dirty bits in the event of a migration failure, we
            // need only offer pages that have their dirty bit set.
            _ => RamOfferDiscipline::OfferDirty,
        };
        self.offer_ram(vmm_ram_range, req_ram_range, offer_discipline).await?;

        loop {
            let m = self.read_msg().await?;
            trace!(self.log(), "ram_push: source xfer phase recvd {:?}", m);
            match m {
                codec::Message::MemDone => break,
                codec::Message::MemFetch(start, end, bits) => {
                    if !memx::validate_bitmap(start, end, &bits) {
                        error!(self.log(), "invalid bitmap");
                        return Err(MigrateError::Phase);
                    }

                    // XXX: We should do stricter validation on the fetch
                    // request here.  For instance, we shouldn't "push" MMIO
                    // space or non-existent RAM regions.  While we de facto
                    // do not because of the way access is implemented, we
                    // should probably disallow it at the protocol level.
                    self.xfer_ram(start, end, &bits).await?;
                    probes::migrate_xfer_ram_region!(|| {
                        let bits = BitSlice::<_, Lsb0>::from_slice(&bits);
                        let pages = bits.count_ones() as u64;
                        (
                            pages,
                            pages * PAGE_SIZE as u64,
                            match phase {
                                MigratePhase::RamPushPrePause => 0,
                                MigratePhase::RamPushPostPause => 1,
                                _ => unreachable!(),
                            },
                        )
                    });
                }
                _ => return Err(MigrateError::UnexpectedMessage),
            };
        }
        info!(self.log(), "ram_push: done sending ram");
        self.update_state(MigrationState::Pause);
        Ok(())
    }

    async fn offer_ram(
        &mut self,
        vmm_ram_range: RangeInclusive<GuestAddr>,
        req_ram_range: Range<u64>,
        offer_discipline: RamOfferDiscipline,
    ) -> Result<(), MigrateError> {
        info!(
            self.log(),
            "offering ram";
            "discipline" => ?offer_discipline,
            "can_redirty_pages" => self.dirt.is_some(),
        );
        let vmm_ram_start = *vmm_ram_range.start();
        let vmm_ram_end = *vmm_ram_range.end();
        let mut bits = [0u8; PAGE_BITMAP_SIZE];
        let req_start_gpa = req_ram_range.start;
        let req_end_gpa = req_ram_range.end;
        let start_gpa = req_start_gpa.max(vmm_ram_start.0);

        // The RAM bounds reported to this routine set the end of the range to
        // the last valid address in the address space (e.g. 0x6FFF for a
        // one-page range beginning at 0x6000), but this routine's callees
        // expect the end address to be the first invalid (page-aligned) address
        // (in our example, 0x7000 instead of 0x6FFF). Correct that here.
        //
        // N.B. This assumes that the guest address space is small enough to
        //      add 1 in this fashion without overflowing, i.e. the last valid
        //      GPA cannot be `u64::MAX`.
        let end_gpa = req_end_gpa.min(vmm_ram_end.0);
        assert!(end_gpa < u64::MAX);
        let end_gpa = end_gpa + 1;

        let step = bits.len() * 8 * PAGE_SIZE;
        for gpa in (start_gpa..end_gpa).step_by(step) {
            let mut pages_offered = 0;
            // Always capture the dirty page mask even if the offer discipline
            // says to offer all pages. This ensures that pages that are
            // transferred now and not touched again will not be offered again
            // by a subsequent phase.
            self.track_dirty(GuestAddr(gpa), &mut bits).await?;

            match offer_discipline {
                RamOfferDiscipline::OfferAll => {
                    for byte in bits.iter_mut() {
                        *byte = 0xff;
                    }
                    pages_offered = PAGE_BITMAP_SIZE * 8;
                }
                RamOfferDiscipline::OfferDirty => {
                    let bits = BitSlice::<_, Lsb0>::from_slice(&bits);
                    let dirty_pages = bits.count_ones();
                    if dirty_pages == 0 {
                        continue;
                    }

                    pages_offered += dirty_pages;

                    // If we're on a bhyve version that supports
                    // VM_NPT_OPERATION, we'll be able to put the dirty bits
                    // back in the event of a migration failure. Therefore,
                    // we need to hang onto any bytes and their indices so
                    // that we can rebuild the dirty page mask later, if
                    // necessary.
                    //
                    // If bhyve doesn't support VM_NPT_OPERATION, no sense
                    // hanging onto this. We'll just have to offer all pages
                    // in the initial RAM Offer phase, instead.
                    if let Some(ref mut dirt) = self.dirt {
                        let saved = dirt
                            .entry(GuestAddr(gpa))
                            .or_insert_with(|| [0u8; PAGE_BITMAP_SIZE]);
                        let saved = BitSlice::<_, Lsb0>::from_slice_mut(saved);
                        *saved |= bits;
                    }
                }
            }

            let end = end_gpa.min(gpa + step as u64);
            info!(
                self.log(),
                "ram_push: offering {pages_offered} pages between {gpa:#x} and {end:#x}"
            );
            if pages_offered > 0 {
                self.send_msg(memx::make_mem_offer(gpa, end, &bits)).await?;
            }
        }
        self.send_msg(codec::Message::MemEnd(req_start_gpa, req_end_gpa)).await
    }

    async fn xfer_ram(
        &mut self,
        start: u64,
        end: u64,
        bits: &[u8],
    ) -> Result<(), MigrateError> {
        info!(self.log(), "ram_push: xfer RAM between {start:#x} and {end:#x}",);
        self.send_msg(memx::make_mem_xfer(start, end, bits)).await?;
        for addr in PageIter::new(start, end, bits) {
            let mut byte_buffer = [0u8; PAGE_SIZE];
            {
                let mut bytes = GuestData::from(byte_buffer.as_mut_slice());
                self.read_guest_mem(GuestAddr(addr), &mut bytes).await?;
            }
            self.send_msg(codec::Message::Page(byte_buffer.into())).await?;
            probes::migrate_xfer_ram_page!(|| (addr, PAGE_SIZE as u64));
        }
        Ok(())
    }

    async fn pause(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Pause);
        // Ask the instance to begin transitioning to the paused state
        // This will inform each device to pause.
        info!(self.log(), "Pausing devices");
        self.pause_vm().await;
        Ok(())
    }

    async fn device_state(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Device);
        let mut device_states = vec![];
        {
            let objects = self.vm.lock_shared().await;
            let migrate_ctx =
                MigrateCtx { mem: &objects.access_mem().unwrap() };

            // Collect together the serialized state for all the devices
            objects.for_each_device_fallible(|name, devop| {
                let mut dev = Device {
                    instance_name: name.to_string(),
                    payload: Vec::new(),
                };
                match devop.migrate() {
                    Migrator::NonMigratable => {
                        error!(self.log(),
                            "Can't migrate instance with non-migratable device ({})",
                            name);
                        return Err(MigrateError::DeviceState(
                                MigrateStateError::NonMigratable.to_string()));
                    },
                    // No device state needs to be trasmitted for 'Empty' devices
                    Migrator::Empty => {},
                    Migrator::Single(mech) => {
                        let out = mech.export(&migrate_ctx)?;
                        dev.payload.push(DevicePayload {
                            kind: out.kind.to_owned(),
                            version: out.version,
                            data: ron::ser::to_string(&out.payload)
                                .map_err(codec::ProtocolError::from)?,
                        });
                        device_states.push(dev);
                    }
                    Migrator::Multi(mech) => {
                        let mut outputs = PayloadOutputs::new();
                        mech.export(&mut outputs, &migrate_ctx)?;

                        for part in outputs {
                            dev.payload.push(DevicePayload {
                                kind: part.kind.to_owned(),
                                version: part.version,
                                data: ron::ser::to_string(&part.payload)
                                    .map_err(codec::ProtocolError::from)?,
                            });
                        }
                        device_states.push(dev);
                    }
                }
                Ok(())
            })?;
        }

        info!(self.log(), "Device States: {device_states:#?}");

        self.send_msg(codec::Message::Serialized(
            ron::ser::to_string(&device_states)
                .map_err(codec::ProtocolError::from)?,
        ))
        .await?;

        self.send_msg(codec::Message::Okay).await?;
        self.read_ok().await
    }

    // Read and send over the time data
    async fn time_data(&mut self) -> Result<(), MigrateError> {
        let vmm_hdl = &self.vm.lock_shared().await.vmm_hdl().clone();
        let vm_time_data =
            vmm::time::export_time_data(vmm_hdl).map_err(|e| {
                MigrateError::TimeData(format!(
                    "VMM Time Data export error: {}",
                    e
                ))
            })?;
        info!(self.log(), "VMM Time Data: {:#?}", vm_time_data);

        let time_data_serialized = ron::ser::to_string(&vm_time_data)
            .map_err(codec::ProtocolError::from)?;
        info!(self.log(), "VMM Time Data: {:#?}", time_data_serialized);
        self.send_msg(codec::Message::Serialized(time_data_serialized)).await?;

        self.read_ok().await
    }

    async fn ram_pull(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::RamPush);
        let m = self.read_msg().await?;
        info!(self.log(), "ram_pull: got query {:?}", m);
        self.update_state(MigrationState::Pause);
        self.update_state(MigrationState::RamPushDirty);
        self.send_msg(codec::Message::MemEnd(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_pull: got done {:?}", m);
        Ok(())
    }

    async fn server_state(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Server);
        let remote_addr = match self.read_msg().await? {
            Message::Serialized(s) => {
                ron::from_str(&s).map_err(codec::ProtocolError::from)?
            }
            _ => return Err(MigrateError::UnexpectedMessage),
        };
        let com1_history = self
            .vm
            .lock_shared()
            .await
            .com1()
            .export_history(remote_addr)
            .await?;
        self.send_msg(codec::Message::Serialized(com1_history)).await?;
        self.read_ok().await
    }

    async fn finish(&mut self) -> Result<(), MigrateError> {
        // Wait for the destination to acknowledge that it's ready to run the
        // VM.
        self.read_ok().await?;

        // Hand control over to the destination. If this send fails, the
        // destination won't run the VM and it can resume here.
        //
        // N.B. After this message is sent, this Propolis (and any of its
        //      overseers) must assume that the destination has begun running
        //      the guest.
        self.send_msg(codec::Message::Okay).await?;

        // Now that handoff is complete, publish that the migration has
        // succeeded.
        self.update_state(MigrationState::Finish);

        // This VMM is going away, so if any guest memory is still dirty, it
        // won't be transferred. Assert that there is no such memory.
        //
        // The unwraps in the block below amount to assertions that the VMM
        // exists at this point (it should). Note that returning an error here
        // is not permitted because that will cause migration to unwind and the
        // VM to resume, which is forbidden at this point (see above).
        let vmm_range = self.vmm_ram_bounds().await.unwrap();
        let mut bits = [0u8; PAGE_BITMAP_SIZE];
        let step = bits.len() * 8 * PAGE_SIZE;
        for gpa in (vmm_range.start().0..vmm_range.end().0).step_by(step) {
            self.track_dirty(GuestAddr(gpa), &mut bits).await.unwrap();
            let pages_left_behind =
                BitSlice::<_, Lsb0>::from_slice(&bits).count_ones() as u64;
            assert_eq!(
                0,
                pages_left_behind,
                "{pages_left_behind} dirty pages left behind between {:#x}..{:#x}",
                gpa,
                gpa + step as u64,
            );
        }

        Ok(())
    }

    async fn read_msg(&mut self) -> Result<codec::Message, MigrateError> {
        self.conn
            .next()
            .await
            .ok_or_else(|| {
                codec::ProtocolError::Io(io::Error::from(
                    io::ErrorKind::BrokenPipe,
                ))
            })?
            .map_err(codec::ProtocolError::WebsocketError)
            // convert tungstenite::Message to codec::Message
            .and_then(std::convert::TryInto::try_into)
            // If this is an error message, lift that out
            .map(|msg| match msg {
                codec::Message::Error(err) => {
                    error!(
                        self.log(),
                        "migration failed due to error from target: {err}"
                    );
                    Err(MigrateError::RemoteError(
                        MigrateRole::Destination,
                        err.to_string(),
                    ))
                }
                msg => Ok(msg),
            })?
    }

    async fn read_ok(&mut self) -> Result<(), MigrateError> {
        match self.read_msg().await? {
            codec::Message::Okay => Ok(()),
            msg => {
                error!(self.log(), "expected `Okay` but received: {msg:?}");
                Err(MigrateError::UnexpectedMessage)
            }
        }
    }

    async fn read_mem_query(&mut self) -> Result<Range<u64>, MigrateError> {
        match self.read_msg().await? {
            codec::Message::MemQuery(start, end) => {
                if start % PAGE_SIZE as u64 != 0
                    || (end % PAGE_SIZE as u64 != 0 && end != !0)
                {
                    return Err(MigrateError::Phase);
                }
                Ok(start..end)
            }
            msg => {
                error!(self.log(), "expected `MemQuery` but received: {msg:?}");
                Err(MigrateError::UnexpectedMessage)
            }
        }
    }

    async fn send_msg(
        &mut self,
        m: codec::Message,
    ) -> Result<(), MigrateError> {
        Ok(self.conn.send(m.try_into()?).await?)
    }

    async fn vmm_ram_bounds(
        &mut self,
    ) -> Result<RangeInclusive<GuestAddr>, MigrateError> {
        let objects = self.vm.lock_shared().await;
        let memctx = objects.access_mem().unwrap();
        memctx.mem_bounds().ok_or(MigrateError::InvalidInstanceState)
    }

    async fn track_dirty(
        &mut self,
        start_gpa: GuestAddr,
        bits: &mut [u8],
    ) -> Result<(), MigrateError> {
        self.vm
            .lock_shared()
            .await
            .vmm_hdl()
            .track_dirty_pages(start_gpa.0, bits)
            .map_err(|_| MigrateError::InvalidInstanceState)
    }

    async fn read_guest_mem(
        &mut self,
        addr: GuestAddr,
        buf: &mut GuestData<&mut [u8]>,
    ) -> Result<(), MigrateError> {
        let objects = self.vm.lock_shared().await;
        let memctx = objects.access_mem().unwrap();
        let len = buf.len();
        memctx.direct_read_into(addr, buf, len);
        Ok(())
    }
}
