use futures::{future, SinkExt, StreamExt};
use hyper::upgrade::Upgraded;
use propolis::common::GuestAddr;
use propolis::instance::{MigrateRole, ReqState};
use propolis::inventory::Order;
use propolis::migrate::{MigrateStateError, Migrator};
use slog::{error, info};
use std::io;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;
use std::time::Duration;
use tokio::{task, time};
use tokio_util::codec::Framed;

use crate::migrate::codec::{self, LiveMigrationFramer};
use crate::migrate::memx;
use crate::migrate::preamble::Preamble;
use crate::migrate::{
    Device, MigrateContext, MigrateError, MigrationState, PageIter,
};

pub async fn migrate(
    mctx: Arc<MigrateContext>,
    conn: Upgraded,
) -> Result<(), MigrateError> {
    let mut proto = SourceProtocol::new(mctx, conn);

    if let Err(err) = proto.run().await {
        proto.mctx.set_state(MigrationState::Error).await;
        // We encountered an error, try to inform the remote before bailing
        // Note, we don't use `?` here as this is a best effort and we don't
        // want an error encountered during this send to shadow the run error
        // from the caller.
        let _ = proto.conn.send(codec::Message::Error(err.clone())).await;
        return Err(err);
    }

    Ok(())
}

struct SourceProtocol {
    /// The migration context which also contains the Instance handle.
    mctx: Arc<MigrateContext>,

    /// Transport to the destination Instance.
    conn: Framed<Upgraded, LiveMigrationFramer>,
}

impl SourceProtocol {
    fn new(mctx: Arc<MigrateContext>, conn: Upgraded) -> Self {
        let codec_log = mctx.log.new(slog::o!());
        Self {
            mctx,
            conn: Framed::new(conn, LiveMigrationFramer::new(codec_log)),
        }
    }

    fn log(&self) -> &slog::Logger {
        &self.mctx.log
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
        self.end()?;
        Ok(())
    }

    fn start(&mut self) {
        info!(self.log(), "Entering Source Migration Task");
    }

    async fn sync(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Sync).await;
        let preamble = Preamble::new(self.mctx.instance_spec.clone());
        let s = ron::ser::to_string(&preamble)
            .map_err(codec::ProtocolError::from)?;
        self.send_msg(codec::Message::Serialized(s)).await?;
        self.read_ok().await
    }

    async fn ram_push(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::RamPush).await;
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
        self.mctx.set_state(MigrationState::Pause).await;
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
        self.mctx.set_state(MigrationState::Pause).await;

        // Grab a reference to all the devices that are a part of this Instance
        let mut devices = vec![];
        let _ =
            self.mctx.instance.inv().for_each_node(Order::Post, |_, rec| {
                devices.push((rec.name().to_owned(), Arc::clone(rec.entity())));
                Ok::<_, ()>(())
            });

        // Ask the instance to begin transitioning to the paused state
        // This will inform each device to pause.
        info!(self.log(), "Pausing devices");
        let (pause_tx, pause_rx) = std::sync::mpsc::channel();
        self.mctx
            .instance
            .migrate_pause(self.mctx.async_ctx.context_id(), pause_rx)?
            .notified()
            .await;

        // Ask each device for a future indicating they've finishing pausing
        let mut migrate_ready_futs = vec![];
        for (name, device) in &devices {
            let log = self.log().new(slog::o!("device" => name.clone()));
            let device = Arc::clone(device);
            let pause_fut = device.paused();
            migrate_ready_futs.push(task::spawn(async move {
                if let Err(_) =
                    time::timeout(Duration::from_secs(2), pause_fut).await
                {
                    error!(log, "Timed out pausing device");
                    return Err(device);
                }
                info!(log, "Paused device");
                Ok(())
            }));
        }

        // Now we wait for all the devices to have paused
        let pause = future::join_all(migrate_ready_futs)
            .await
            // Hoist out the JoinError's
            .into_iter()
            .collect::<Result<Vec<_>, _>>();
        let timed_out = match pause {
            Ok(future_res) => {
                // Grab just the ones that failed
                future_res
                    .into_iter()
                    .filter(Result::is_err)
                    .map(Result::unwrap_err)
                    .collect::<Vec<_>>()
            }
            Err(err) => {
                error!(
                    self.log(),
                    "joining paused devices future failed: {err}"
                );
                return Err(MigrateError::SourcePause);
            }
        };

        // Bail out if any devices timed out
        // TODO: rollback already paused devices
        if !timed_out.is_empty() {
            error!(self.log(), "Failed to pause all devices: {timed_out:?}");
            return Err(MigrateError::SourcePause);
        }

        // Inform the instance state machine we're done pausing
        pause_tx.send(()).unwrap();

        Ok(())
    }

    async fn device_state(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Device).await;

        let dispctx = self
            .mctx
            .async_ctx
            .dispctx()
            .await
            .ok_or_else(|| MigrateError::InstanceNotInitialized)?;

        // Collect together the serialized state for all the devices
        let mut device_states = vec![];
        self.mctx.instance.inv().for_each_node(Order::Pre, |_, rec| {
            let entity = rec.entity();
            match entity.migrate() {
                Migrator::NonMigratable => {
                    error!(self.log(), "Can't migrate instance with non-migratable device ({})", rec.name());
                    return Err(MigrateError::DeviceState(MigrateStateError::NonMigratable));
                },
                // No device state needs to be trasmitted for 'Simple' devices
                Migrator::Simple => {},
                Migrator::Custom(migrate) => {
                    let payload = migrate.export(&dispctx);
                    device_states.push(Device {
                        instance_name: rec.name().to_owned(),
                        payload: ron::ser::to_string(&payload)
                            .map_err(codec::ProtocolError::from)?,
                    });
                },
            }
            Ok(())
        })?;
        drop(dispctx);

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
        self.mctx.set_state(MigrationState::Arch).await;
        self.read_ok().await?;
        self.send_msg(codec::Message::Okay).await
    }

    async fn ram_pull(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::RamPush).await;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_pull: got query {:?}", m);
        self.mctx.set_state(MigrationState::Pause).await;
        self.mctx.set_state(MigrationState::RamPushDirty).await;
        self.send_msg(codec::Message::MemEnd(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_pull: got done {:?}", m);
        Ok(())
    }

    async fn finish(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Finish).await;
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

    fn end(&mut self) -> Result<(), MigrateError> {
        self.mctx.instance.set_target_state(ReqState::Halt)?;
        info!(self.log(), "Source Migration Successful");
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
        Ok(self.conn.send(m).await?)
    }

    async fn vmm_ram_bounds(
        &mut self,
    ) -> Result<RangeInclusive<GuestAddr>, MigrateError> {
        let memctx = self
            .mctx
            .async_ctx
            .dispctx()
            .await
            .ok_or(MigrateError::InstanceNotInitialized)?
            .mctx
            .memctx();
        memctx.mem_bounds().ok_or(MigrateError::InvalidInstanceState)
    }

    async fn track_dirty(
        &mut self,
        start_gpa: GuestAddr,
        bits: &mut [u8],
    ) -> Result<(), MigrateError> {
        let handle = self
            .mctx
            .async_ctx
            .dispctx()
            .await
            .ok_or(MigrateError::InstanceNotInitialized)?
            .mctx
            .hdl();
        handle
            .track_dirty_pages(start_gpa.0, bits)
            .map_err(|_| MigrateError::InvalidInstanceState)
    }

    async fn read_guest_mem(
        &mut self,
        addr: GuestAddr,
        buf: &mut [u8],
    ) -> Result<(), MigrateError> {
        let memctx = self
            .mctx
            .async_ctx
            .dispctx()
            .await
            .ok_or(MigrateError::InstanceNotInitialized)?
            .mctx
            .memctx();
        let len = buf.len();
        memctx.direct_read_into(addr, buf, len);
        Ok(())
    }
}
