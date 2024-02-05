// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use futures::{SinkExt, StreamExt};
use propolis::common::{GuestAddr, PAGE_SIZE};
use propolis::migrate::{
    MigrateCtx, MigrateStateError, Migrator, PayloadOutputs,
};
use propolis::vmm;
use slog::{error, info, trace};
use std::convert::TryInto;
use std::io;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;

use crate::migrate::codec;
use crate::migrate::codec::Message;
use crate::migrate::memx;
use crate::migrate::preamble::Preamble;
use crate::migrate::probes;
use crate::migrate::protocol::Protocol;
use crate::migrate::{
    Device, DevicePayload, MigrateError, MigratePhase, MigrateRole,
    MigrationState, PageIter,
};
use crate::vm::{MigrateSourceCommand, MigrateSourceResponse, VmController};

/// Specifies which pages should be offered during a RAM transfer phase.
#[derive(Debug)]
enum RamOfferDiscipline {
    /// Offer all pages irrespective of whether they are dirty.
    OfferAll,

    /// Offer only pages that the hypervisor are marked as dirty.
    OfferDirty,
}

pub async fn migrate<T: AsyncRead + AsyncWrite + Unpin + Send>(
    vm_controller: Arc<VmController>,
    command_tx: tokio::sync::mpsc::Sender<MigrateSourceCommand>,
    response_rx: tokio::sync::mpsc::Receiver<MigrateSourceResponse>,
    conn: WebSocketStream<T>,
    protocol: super::protocol::Protocol,
) -> Result<(), MigrateError> {
    let err_tx = command_tx.clone();
    let mut proto = match protocol {
        Protocol::RonV0 => {
            SourceProtocol::new(vm_controller, command_tx, response_rx, conn)
        }
    };

    if let Err(err) = proto.run().await {
        err_tx
            .send(MigrateSourceCommand::UpdateState(MigrationState::Error))
            .await
            .unwrap();

        // We encountered an error, try to inform the remote before bailing
        // Note, we don't use `?` here as this is a best effort and we don't
        // want an error encountered during this send to shadow the run error
        // from the caller.
        let _ = proto.send_msg(codec::Message::Error(err.clone())).await;
        return Err(err);
    }

    Ok(())
}

struct SourceProtocol<T: AsyncRead + AsyncWrite + Unpin + Send> {
    /// The VM controller for the instance of interest.
    vm_controller: Arc<VmController>,

    /// The channel to use to send messages to the state worker coordinating
    /// this migration.
    command_tx: tokio::sync::mpsc::Sender<MigrateSourceCommand>,

    /// The channel to use to receive messages from the state worker
    /// coordinating this migration.
    response_rx: tokio::sync::mpsc::Receiver<MigrateSourceResponse>,

    /// Transport to the destination Instance.
    conn: WebSocketStream<T>,
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> SourceProtocol<T> {
    fn new(
        vm_controller: Arc<VmController>,
        command_tx: tokio::sync::mpsc::Sender<MigrateSourceCommand>,
        response_rx: tokio::sync::mpsc::Receiver<MigrateSourceResponse>,
        conn: WebSocketStream<T>,
    ) -> Self {
        Self { vm_controller, command_tx, response_rx, conn }
    }

    fn log(&self) -> &slog::Logger {
        self.vm_controller.log()
    }

    async fn update_state(&mut self, state: MigrationState) {
        // When migrating into an instance, the VM state worker blocks waiting
        // for the disposition of the migration attempt, so the channel should
        // never be closed before the attempt completes.
        self.command_tx
            .send(MigrateSourceCommand::UpdateState(state))
            .await
            .unwrap();
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

        self.run_phase(MigratePhase::MigrateSync).await?;
        self.run_phase(MigratePhase::RamPushPrePause).await?;
        self.run_phase(MigratePhase::Pause).await?;
        self.run_phase(MigratePhase::RamPushPostPause).await?;
        self.run_phase(MigratePhase::TimeData).await?;
        self.run_phase(MigratePhase::DeviceState).await?;
        self.run_phase(MigratePhase::RamPull).await?;
        self.run_phase(MigratePhase::ServerState).await?;
        self.run_phase(MigratePhase::Finish).await?;

        info!(self.log(), "Source Migration Successful");
        Ok(())
    }

    async fn sync(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Sync).await;
        let preamble =
            Preamble::new(self.vm_controller.instance_spec().await.clone());
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
                self.update_state(MigrationState::RamPush).await
            }
            MigratePhase::RamPushPostPause => {
                self.update_state(MigrationState::RamPushDirty).await
            }
            _ => unreachable!("should only push RAM in a RAM push phase"),
        }

        let vmm_ram_range = self.vmm_ram_bounds().await?;
        let req_ram_range = self.read_mem_query().await?;
        info!(
            self.log(),
            "ram_push ({:?}): got query for range {:?}, vm range {:?}",
            phase,
            req_ram_range,
            vmm_ram_range
        );

        // TODO(#387): Ideally, both the pre-pause and post-pause phases would
        // offer just dirty pages. To do this safely, the source must remember
        // all the pages it has ever offered to any target so that they can be
        // re-offered if migration fails and is later retried.
        //
        // Offering all pages before pausing guarantees that all modified pages
        // will be transferred without having to do any extra tracking (but uses
        // host CPU time and network bandwidth inefficiently).
        self.offer_ram(
            vmm_ram_range,
            req_ram_range,
            match phase {
                MigratePhase::RamPushPrePause => RamOfferDiscipline::OfferAll,
                MigratePhase::RamPushPostPause => {
                    RamOfferDiscipline::OfferDirty
                }
                _ => unreachable!(),
            },
        )
        .await?;

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
                        use bitvec::prelude::{BitSlice, Lsb0};
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
        self.update_state(MigrationState::Pause).await;
        Ok(())
    }

    async fn offer_ram(
        &mut self,
        vmm_ram_range: RangeInclusive<GuestAddr>,
        req_ram_range: Range<u64>,
        offer_discipline: RamOfferDiscipline,
    ) -> Result<(), MigrateError> {
        info!(self.log(), "offering ram"; "discipline" => ?offer_discipline);
        let vmm_ram_start = *vmm_ram_range.start();
        let vmm_ram_end = *vmm_ram_range.end();
        let mut bits = [0u8; 4096];
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
            // Always capture the dirty page mask even if the offer discipline
            // says to offer all pages. This ensures that pages that are
            // transferred now and not touched again will not be offered again.
            self.track_dirty(GuestAddr(gpa), &mut bits).await?;
            match offer_discipline {
                RamOfferDiscipline::OfferAll => {
                    for byte in bits.iter_mut() {
                        *byte = 0xff;
                    }
                }
                RamOfferDiscipline::OfferDirty => {
                    if bits.iter().all(|&b| b == 0) {
                        continue;
                    }
                }
            }

            let end = end_gpa.min(gpa + step as u64);
            self.send_msg(memx::make_mem_offer(gpa, end, &bits)).await?;
        }
        self.send_msg(codec::Message::MemEnd(req_start_gpa, req_end_gpa)).await
    }

    async fn xfer_ram(
        &mut self,
        start: u64,
        end: u64,
        bits: &[u8],
    ) -> Result<(), MigrateError> {
        info!(self.log(), "ram_push: xfer RAM between {} and {}", start, end);
        self.send_msg(memx::make_mem_xfer(start, end, bits)).await?;
        for addr in PageIter::new(start, end, bits) {
            let mut bytes = [0u8; PAGE_SIZE];
            self.read_guest_mem(GuestAddr(addr), &mut bytes).await?;
            self.send_msg(codec::Message::Page(bytes.into())).await?;
            probes::migrate_xfer_ram_page!(|| (addr, PAGE_SIZE as u64));
        }
        Ok(())
    }

    async fn pause(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Pause).await;
        // Ask the instance to begin transitioning to the paused state
        // This will inform each device to pause.
        info!(self.log(), "Pausing devices");
        self.command_tx.send(MigrateSourceCommand::Pause).await.unwrap();
        let resp = self.response_rx.recv().await.unwrap();
        match resp {
            MigrateSourceResponse::Pause(Ok(())) => Ok(()),
            _ => {
                info!(
                    self.log(),
                    "Unexpected pause response from state worker: {:?}", resp
                );
                Err(MigrateError::SourcePause)
            }
        }
    }

    async fn device_state(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Device).await;
        let mut device_states = vec![];
        {
            let machine = self.vm_controller.machine();
            let migrate_ctx =
                MigrateCtx { mem: &machine.acc_mem.access().unwrap() };

            // Collect together the serialized state for all the devices
            self.vm_controller.for_each_device_fallible(|name, devop| {
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
        let vmm_hdl = &self.vm_controller.machine().hdl.clone();
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
        self.update_state(MigrationState::RamPush).await;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_pull: got query {:?}", m);
        self.update_state(MigrationState::Pause).await;
        self.update_state(MigrationState::RamPushDirty).await;
        self.send_msg(codec::Message::MemEnd(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_pull: got done {:?}", m);
        Ok(())
    }

    async fn server_state(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Server).await;
        let remote_addr = match self.read_msg().await? {
            Message::Serialized(s) => {
                ron::from_str(&s).map_err(codec::ProtocolError::from)?
            }
            _ => return Err(MigrateError::UnexpectedMessage),
        };
        let com1_history =
            self.vm_controller.com1().export_history(remote_addr).await?;
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
        self.update_state(MigrationState::Finish).await;

        // This VMM is going away, so if any guest memory is still dirty, it
        // won't be transferred. Assert that there is no such memory.
        //
        // The unwraps in the block below amount to assertions that the VMM
        // exists at this point (it should). Note that returning an error here
        // is not permitted because that will cause migration to unwind and the
        // VM to resume, which is forbidden at this point (see above).
        let vmm_range = self.vmm_ram_bounds().await.unwrap();
        let mut bits = [0u8; 4096];
        let step = bits.len() * 8 * PAGE_SIZE;
        for gpa in (vmm_range.start().0..vmm_range.end().0).step_by(step) {
            self.track_dirty(GuestAddr(gpa), &mut bits).await.unwrap();
            assert!(bits.iter().all(|&b| b == 0));
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
                    error!(self.log(), "remote error: {err}");
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
        let machine = self.vm_controller.machine();
        let memctx = machine.acc_mem.access().unwrap();
        memctx.mem_bounds().ok_or(MigrateError::InvalidInstanceState)
    }

    async fn track_dirty(
        &mut self,
        start_gpa: GuestAddr,
        bits: &mut [u8],
    ) -> Result<(), MigrateError> {
        self.vm_controller
            .machine()
            .hdl
            .track_dirty_pages(start_gpa.0, bits)
            .map_err(|_| MigrateError::InvalidInstanceState)
    }

    async fn read_guest_mem(
        &mut self,
        addr: GuestAddr,
        buf: &mut [u8],
    ) -> Result<(), MigrateError> {
        let machine = self.vm_controller.machine();
        let memctx = machine.acc_mem.access().unwrap();
        let len = buf.len();
        memctx.direct_read_into(addr, buf, len);
        Ok(())
    }
}
