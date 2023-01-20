use futures::{SinkExt, StreamExt};
use propolis::common::GuestAddr;
use propolis::inventory::Order;
use propolis::migrate::{MigrateCtx, MigrateStateError, Migrator};
use slog::{error, info};
use std::convert::TryInto;
use std::io;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;

use crate::migrate::codec;
use crate::migrate::memx;
use crate::migrate::preamble::Preamble;
use crate::migrate::{
    Device, MigrateError, MigrateRole, MigrationState, PageIter,
};
use crate::vm::{MigrateSourceCommand, MigrateSourceResponse, VmController};

pub async fn migrate<T: AsyncRead + AsyncWrite + Unpin + Send>(
    vm_controller: Arc<VmController>,
    command_tx: tokio::sync::mpsc::Sender<MigrateSourceCommand>,
    response_rx: tokio::sync::mpsc::Receiver<MigrateSourceResponse>,
    conn: WebSocketStream<T>,
) -> Result<(), MigrateError> {
    let err_tx = command_tx.clone();
    let mut proto =
        SourceProtocol::new(vm_controller, command_tx, response_rx, conn);

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

    async fn run(&mut self) -> Result<(), MigrateError> {
        self.start();
        self.sync().await?;

        // TODO: Optimize RAM transfer so that most memory can be transferred
        // prior to pausing.
        self.pause().await?;
        self.ram_push().await?;
        self.device_state().await?;
        self.arch_state().await?;
        self.ram_pull().await?;
        self.finish().await?;
        self.end();
        Ok(())
    }

    fn start(&mut self) {
        info!(self.log(), "Entering Source Migration Task");
    }

    async fn sync(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Sync).await;
        let preamble =
            Preamble::new(self.vm_controller.instance_spec().clone());
        let s = ron::ser::to_string(&preamble)
            .map_err(codec::ProtocolError::from)?;
        self.send_msg(codec::Message::Serialized(s)).await?;
        self.read_ok().await
    }

    async fn ram_push(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::RamPush).await;
        let vmm_ram_range = self.vmm_ram_bounds().await?;
        let req_ram_range = self.read_mem_query().await?;
        info!(
            self.log(),
            "ram_push: got query for range {:?}, vm range {:?}",
            req_ram_range,
            vmm_ram_range
        );
        self.offer_ram(vmm_ram_range, req_ram_range).await?;

        loop {
            let m = self.read_msg().await?;
            info!(self.log(), "ram_push: source xfer phase recvd {:?}", m);
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
    ) -> Result<(), MigrateError> {
        info!(self.log(), "offering ram");
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

        let step = bits.len() * 8 * 4096;
        for gpa in (start_gpa..end_gpa).step_by(step) {
            self.track_dirty(GuestAddr(gpa), &mut bits).await?;
            if bits.iter().all(|&b| b == 0) {
                continue;
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
            let mut bytes = [0u8; 4096];
            self.read_guest_mem(GuestAddr(addr), &mut bytes).await?;
            self.send_msg(codec::Message::Page(bytes.into())).await?;
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
            let instance_guard = self.vm_controller.instance().lock();
            let migrate_ctx = MigrateCtx {
                mem: &instance_guard.machine().acc_mem.access().unwrap(),
            };

            // Collect together the serialized state for all the devices
            instance_guard.inventory().for_each_node(Order::Pre, |_, rec| {
                let entity = rec.entity();
                match entity.migrate() {
                    Migrator::NonMigratable => {
                        error!(self.log(), "Can't migrate instance with non-migratable device ({})", rec.name());
                        return Err(MigrateError::DeviceState(MigrateStateError::NonMigratable));
                    },
                    // No device state needs to be trasmitted for 'Simple' devices
                    Migrator::Simple => {},
                    Migrator::Custom(migrate) => {
                        let payload = migrate.export(&migrate_ctx);
                        device_states.push(Device {
                            instance_name: rec.name().to_owned(),
                            payload: ron::ser::to_string(&payload)
                                .map_err(codec::ProtocolError::from)?,
                        });
                    },
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

    async fn arch_state(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Arch).await;
        self.read_ok().await?;
        self.send_msg(codec::Message::Okay).await
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

    async fn finish(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Finish).await;
        self.read_ok().await?;
        let _ = self.send_msg(codec::Message::Okay).await; // A failure here is ok.

        // This VMM is going away, so if any guest memory is still dirty, it
        // won't be transferred. Assert that there is no such memory.
        let vmm_range = self.vmm_ram_bounds().await?;
        let mut bits = [0u8; 4096];
        let step = bits.len() * 8 * 4096;
        for gpa in (vmm_range.start().0..vmm_range.end().0).step_by(step) {
            self.track_dirty(GuestAddr(gpa), &mut bits).await?;
            assert!(bits.iter().all(|&b| b == 0));
        }

        Ok(())
    }

    fn end(&mut self) {
        info!(self.log(), "Source Migration Successful");
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
            .map_err(|e| codec::ProtocolError::WebsocketError(e))
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
                if start % 4096 != 0 || (end % 4096 != 0 && end != !0) {
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
        let instance_guard = self.vm_controller.instance().lock();
        let memctx = instance_guard.machine().acc_mem.access().unwrap();
        memctx.mem_bounds().ok_or(MigrateError::InvalidInstanceState)
    }

    async fn track_dirty(
        &mut self,
        start_gpa: GuestAddr,
        bits: &mut [u8],
    ) -> Result<(), MigrateError> {
        let instance_guard = self.vm_controller.instance().lock();
        instance_guard
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
        let instance_guard = self.vm_controller.instance().lock();
        let memctx = instance_guard.machine().acc_mem.access().unwrap();
        let len = buf.len();
        memctx.direct_read_into(addr, buf, len);
        Ok(())
    }
}
