use bitvec::prelude as bv;
use futures::{SinkExt, StreamExt};
use propolis::common::{GuestAddr, PAGE_SIZE};
use propolis::inventory::Entity;
use propolis::migrate::{
    MigrateCtx, MigrateStateError, Migrator, PayloadOffer, PayloadOffers,
};
use slog::{error, info, trace, warn};
use std::convert::TryInto;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;

use crate::migrate::codec;
use crate::migrate::memx;
use crate::migrate::preamble::Preamble;
use crate::migrate::probes;
use crate::migrate::{
    Device, MigrateError, MigratePhase, MigrateRole, MigrationState, PageIter,
};
use crate::vm::{MigrateTargetCommand, VmController};

/// Launches an attempt to migrate into a supplied instance using the supplied
/// source connection.
pub async fn migrate<T: AsyncRead + AsyncWrite + Unpin + Send>(
    vm_controller: Arc<VmController>,
    command_tx: tokio::sync::mpsc::Sender<MigrateTargetCommand>,
    conn: WebSocketStream<T>,
    local_addr: SocketAddr,
) -> Result<(), MigrateError> {
    let err_tx = command_tx.clone();
    let mut proto =
        DestinationProtocol::new(vm_controller, command_tx, conn, local_addr);

    if let Err(err) = proto.run().await {
        err_tx
            .send(MigrateTargetCommand::UpdateState(MigrationState::Error))
            .await
            .unwrap();

        // We encountered an error, try to inform the remote before bailing
        // Note, we don't use `?` here as this is a best effort and we don't
        // want an error encountered during this send to shadow the run error
        // from the caller.
        if let Ok(e) = codec::Message::Error(err.clone()).try_into() {
            let _ = proto.conn.send(e).await;
        }
        return Err(err);
    }

    Ok(())
}

struct DestinationProtocol<T: AsyncRead + AsyncWrite + Unpin + Send> {
    /// The VM controller for the instance of interest.
    vm_controller: Arc<VmController>,

    /// The channel to use to send messages to the state worker coordinating
    /// this migration.
    command_tx: tokio::sync::mpsc::Sender<MigrateTargetCommand>,

    /// Transport to the source Instance.
    conn: WebSocketStream<T>,

    /// Local propolis-server address
    /// (to inform the source-side where to redirect its clients)
    local_addr: SocketAddr,
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> DestinationProtocol<T> {
    fn new(
        vm_controller: Arc<VmController>,
        command_tx: tokio::sync::mpsc::Sender<MigrateTargetCommand>,
        conn: WebSocketStream<T>,
        local_addr: SocketAddr,
    ) -> Self {
        Self { vm_controller, command_tx, conn, local_addr }
    }

    fn log(&self) -> &slog::Logger {
        self.vm_controller.log()
    }

    async fn update_state(&mut self, state: MigrationState) {
        // When migrating into an instance, the VM state worker blocks waiting
        // for the disposition of the migration attempt, so the channel should
        // never be closed before the attempt completes.
        self.command_tx
            .send(MigrateTargetCommand::UpdateState(state))
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

            // no pause step on the dest side
            MigratePhase::Pause => unreachable!(),
            MigratePhase::RamPushPrePause | MigratePhase::RamPushPostPause => {
                self.ram_push(&step).await
            }
            MigratePhase::DeviceState => self.device_state().await,
            MigratePhase::RamPull => self.ram_pull().await,
            MigratePhase::ServerState => self.server_state().await,
            MigratePhase::Finish => self.finish().await,
        };

        probes::migrate_phase_end!(|| { step.to_string() });

        res
    }

    async fn run(&mut self) -> Result<(), MigrateError> {
        info!(self.log(), "Entering Destination Migration Task");

        self.run_phase(MigratePhase::MigrateSync).await?;

        // The RAM transfer phase runs twice, once before the source pauses and
        // once after. There is no explicit pause phase on the destination,
        // though, so that step does not appear here even though there are
        // pre- and post-pause steps.
        self.run_phase(MigratePhase::RamPushPrePause).await?;
        self.run_phase(MigratePhase::RamPushPostPause).await?;
        self.run_phase(MigratePhase::DeviceState).await?;
        self.run_phase(MigratePhase::RamPull).await?;
        self.run_phase(MigratePhase::ServerState).await?;
        self.run_phase(MigratePhase::Finish).await?;

        info!(self.log(), "Destination Migration Successful");

        Ok(())
    }

    async fn sync(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Sync).await;
        let preamble: Preamble = match self.read_msg().await? {
            codec::Message::Serialized(s) => {
                Ok(ron::de::from_str(&s).map_err(codec::ProtocolError::from)?)
            }
            msg => {
                error!(
                    self.log(),
                    "expected serialized preamble but received: {msg:?}"
                );
                Err(MigrateError::UnexpectedMessage)
            }
        }?;
        info!(self.log(), "Destination read Preamble: {:?}", preamble);
        if let Err(e) =
            preamble.is_migration_compatible(self.vm_controller.instance_spec())
        {
            error!(
                self.log(),
                "Source and destination instance specs incompatible: {}", e
            );
            return Err(MigrateError::InvalidInstanceState);
        }

        self.send_msg(codec::Message::Okay).await
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

        let (dirty, highest) = self.query_ram().await?;
        for (k, region) in dirty.as_raw_slice().chunks(4096).enumerate() {
            if region.iter().all(|&b| b == 0) {
                continue;
            }

            // This is an iteration over chunks of 4,096 bitmap bytes, so
            // (k * 4096) is the offset (into the overall bitmap) of the first
            // byte in the chunk. Multiply this by 8 bits/byte to get a number
            // of bits, then multiply by PAGE_SIZE to get a physical address.
            let start = (k * 4096 * 8 * PAGE_SIZE) as u64;
            let end = start + (region.len() * 8 * PAGE_SIZE) as u64;
            let end = highest.min(end);
            self.send_msg(memx::make_mem_fetch(start, end, region)).await?;
            let m = self.read_msg().await?;
            trace!(
                self.log(),
                "ram_push ({:?}): source xfer phase recvd {:?}",
                m,
                phase
            );
            match m {
                codec::Message::MemXfer(start, end, bits) => {
                    if !memx::validate_bitmap(start, end, &bits) {
                        error!(
                            self.log(),
                            "ram_push ({:?}): MemXfer received bad bitmap",
                            phase
                        );
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
        self.send_msg(codec::Message::MemDone).await?;
        self.update_state(MigrationState::Pause).await;
        Ok(())
    }

    async fn query_ram(
        &mut self,
    ) -> Result<(bv::BitVec<u8, bv::Lsb0>, u64), MigrateError> {
        self.send_msg(codec::Message::MemQuery(0, !0)).await?;

        let mut dirty = bv::BitVec::<u8, bv::Lsb0>::new();
        let mut highest = 0;
        loop {
            let m = self.read_msg().await?;
            trace!(self.log(), "ram_push: source xfer phase recvd {:?}", m);
            match m {
                codec::Message::MemEnd(start, end) => {
                    if start != 0 || end != !0 {
                        error!(self.log(), "ram_push: received bad MemEnd");
                        return Err(MigrateError::Phase);
                    }
                    break;
                }
                codec::Message::MemOffer(start, end, bits) => {
                    if !memx::validate_bitmap(start, end, &bits) {
                        error!(
                            self.log(),
                            "ram_push: MemOffer received bad bitmap"
                        );
                        return Err(MigrateError::Phase);
                    }
                    if end > highest {
                        highest = end;
                    }
                    let start_bit_index = start as usize / PAGE_SIZE;
                    if dirty.len() < start_bit_index {
                        dirty.resize(start_bit_index, false);
                    }
                    dirty.extend_from_raw_slice(&bits);
                }
                _ => return Err(MigrateError::UnexpectedMessage),
            }
        }
        Ok((dirty, highest))
    }

    async fn xfer_ram(
        &mut self,
        start: u64,
        end: u64,
        bits: &[u8],
    ) -> Result<(), MigrateError> {
        info!(self.log(), "ram_push: xfer RAM between {} and {}", start, end);
        for addr in PageIter::new(start, end, bits) {
            let bytes = self.read_page().await?;
            self.write_guest_ram(GuestAddr(addr), &bytes).await?;
        }
        Ok(())
    }

    async fn device_state(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Device).await;

        let devices: Vec<Device> = match self.read_msg().await? {
            codec::Message::Serialized(encoded) => {
                ron::de::from_reader(encoded.as_bytes())
                    .map_err(codec::ProtocolError::from)?
            }
            msg => {
                error!(self.log(), "device_state: unexpected message: {msg:?}");
                return Err(MigrateError::UnexpectedMessage);
            }
        };
        self.read_ok().await?;

        info!(self.log(), "Devices: {devices:#?}");

        {
            let instance_guard = self.vm_controller.instance().lock();
            let inv = instance_guard.inventory();
            let migrate_ctx = MigrateCtx {
                mem: &instance_guard.machine().acc_mem.access().unwrap(),
            };
            for device in devices {
                info!(
                    self.log(),
                    "Applying state to device {}", device.instance_name
                );

                let target = inv
                    .get_by_name(&device.instance_name)
                    .ok_or_else(|| {
                        MigrateError::UnknownDevice(
                            device.instance_name.clone(),
                        )
                    })?;
                self.import_device(&target, &device, &migrate_ctx)?;
            }
        }

        self.send_msg(codec::Message::Okay).await
    }

    fn import_device(
        &self,
        target: &Arc<dyn Entity>,
        device: &Device,
        migrate_ctx: &MigrateCtx,
    ) -> Result<(), MigrateError> {
        match target.migrate() {
            Migrator::NonMigratable => {
                error!(
                    self.log(),
                    "Can't migrate instance with non-migratable \
                               device ({})",
                    device.instance_name
                );
                return Err(MigrateStateError::NonMigratable.into());
            }
            Migrator::Empty => {
                // The source shouldn't be sending devices with empty payloads
                warn!(
                    self.log(),
                    "received unexpected device state for device {}",
                    device.instance_name
                );
            }
            Migrator::Single(mech) => {
                if device.payload.len() != 1 {
                    return Err(MigrateError::DeviceState(format!(
                        "Unexpected payload count {}",
                        device.payload.len()
                    )));
                }

                let payload = &device.payload[0];
                let ron_data = &mut ron::Deserializer::from_str(&payload.data)
                    .map_err(codec::ProtocolError::from)?;
                let clean =
                    Box::new(<dyn erased_serde::Deserializer>::erase(ron_data));
                let offer = PayloadOffer {
                    kind: &payload.kind,
                    version: payload.version,
                    payload: clean,
                };

                mech.import(offer, migrate_ctx)?;
            }
            Migrator::Multi(mech) => {
                // Assembling the collection of PayloadOffers looks a bit more
                // verbose than ideal, but gathering the borrows (those split
                // from Device, and the mutable Deserializer) all at once
                // requires a delicate dance.
                let mut payload_desers: Vec<ron::Deserializer> =
                    Vec::with_capacity(device.payload.len());
                let mut metadata: Vec<(&str, u32)> =
                    Vec::with_capacity(device.payload.len());
                for payload in device.payload.iter() {
                    payload_desers.push(
                        ron::Deserializer::from_str(&payload.data)
                            .map_err(codec::ProtocolError::from)?,
                    );
                    metadata.push((&payload.kind, payload.version));
                }
                let offer_iter = metadata
                    .iter()
                    .zip(payload_desers.iter_mut())
                    .map(|(meta, deser)| PayloadOffer {
                        kind: meta.0,
                        version: meta.1,
                        payload: Box::new(
                            <dyn erased_serde::Deserializer>::erase(deser),
                        ),
                    });

                let mut offer = PayloadOffers::new(offer_iter);
                mech.import(&mut offer, migrate_ctx)?;

                let mut count = 0;
                for offer in offer.remaining() {
                    error!(
                        self.log(),
                        "Unexpected payload - device:{} kind:{} version:{}",
                        &device.instance_name,
                        offer.kind,
                        offer.version,
                    );
                    count += 1;
                }
                if count != 0 {
                    return Err(MigrateError::DeviceState(format!(
                        "Found {} unconsumed payload(s) for device {}",
                        count, &device.instance_name,
                    )));
                }
            }
        }
        Ok(())
    }

    async fn ram_pull(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::RamPull).await;
        self.send_msg(codec::Message::MemQuery(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_pull: got end {:?}", m);
        self.send_msg(codec::Message::MemDone).await
    }

    async fn server_state(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Server).await;
        self.send_msg(codec::Message::Serialized(
            ron::to_string(&self.local_addr)
                .map_err(codec::ProtocolError::from)?,
        ))
        .await?;
        let com1_history = match self.read_msg().await? {
            codec::Message::Serialized(encoded) => encoded,
            msg => {
                error!(self.log(), "server_state: unexpected message: {msg:?}");
                return Err(MigrateError::UnexpectedMessage);
            }
        };

        self.vm_controller
            .com1()
            .import(&com1_history)
            .await
            .map_err(|e| MigrateError::Codec(e.to_string()))?;
        self.send_msg(codec::Message::Okay).await
    }

    async fn finish(&mut self) -> Result<(), MigrateError> {
        // Tell the source this destination is ready to run the VM.
        self.send_msg(codec::Message::Okay).await?;

        // Wait for the source to acknowledge that it's handing control to this
        // destination. If this acknowledgement doesn't arrive, there's no way
        // to be sure the source hasn't decided the migration has failed and
        // that it should resume the VM.
        self.read_ok().await?;

        // Now that control is definitely being transferred, publish that the
        // migration has succeeded.
        self.update_state(MigrationState::Finish).await;
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
            .map(|msg| match msg.try_into()? {
                codec::Message::Error(err) => {
                    error!(self.log(), "remote error: {err}");
                    Err(MigrateError::RemoteError(
                        MigrateRole::Source,
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

    async fn read_page(&mut self) -> Result<Vec<u8>, MigrateError> {
        match self.read_msg().await? {
            codec::Message::Page(bytes) => Ok(bytes),
            _ => Err(MigrateError::UnexpectedMessage),
        }
    }

    async fn send_msg(
        &mut self,
        m: codec::Message,
    ) -> Result<(), MigrateError> {
        Ok(self.conn.send(m.try_into()?).await?)
    }

    async fn write_guest_ram(
        &mut self,
        addr: GuestAddr,
        buf: &[u8],
    ) -> Result<(), MigrateError> {
        let instance_guard = self.vm_controller.instance().lock();
        let memctx = instance_guard.machine().acc_mem.access().unwrap();
        let len = buf.len();
        memctx.write_from(addr, buf, len);
        Ok(())
    }
}
