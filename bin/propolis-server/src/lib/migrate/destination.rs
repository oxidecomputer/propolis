// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use bitvec::prelude as bv;
use futures::{SinkExt, StreamExt};
use propolis::common::{GuestAddr, Lifecycle, PAGE_SIZE};
use propolis::migrate::{
    MigrateCtx, MigrateStateError, Migrator, PayloadOffer, PayloadOffers,
};
use propolis::vmm;
use propolis_api_types::InstanceMigrateInitiateRequest;
use slog::{error, info, trace, warn};
use std::convert::TryInto;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{tungstenite, WebSocketStream};
use uuid::Uuid;

use crate::migrate::codec;
use crate::migrate::memx;
use crate::migrate::preamble::Preamble;
use crate::migrate::probes;
use crate::migrate::{
    Device, MigrateError, MigratePhase, MigrateRole, MigrationState, PageIter,
};
use crate::vm::ensure::{VmEnsureActive, VmEnsureNotStarted};
use crate::vm::objects::VmObjectsShared;
use crate::vm::state_publisher::{ExternalStateUpdate, MigrationStateUpdate};

use super::protocol::Protocol;
use super::MigrateConn;

/// The interface to an arbitrary version of the target half of the live
/// migration protocol.
//
// Use `async_trait` here to help generate a `Send` bound on the futures
// returned by the functions in this trait.
#[async_trait::async_trait]
pub(crate) trait DestinationProtocol<'e> {
    /// Runs live migration as a target, attempting to create a set of VM
    /// objects in the process. On success, returns an "active VM" placeholder
    /// that the caller can use to set up and start a state driver loop.
    async fn run<'ensure>(
        mut self,
        ensure: VmEnsureNotStarted<'ensure>,
    ) -> Result<VmEnsureActive<'ensure>, MigrateError>
    where
        'e: 'ensure;
}

/// Connects to a live migration source using the migration request information
/// in `migrate_info`, then negotiates a protocol version with that source.
/// Returns a [`DestinationProtocol`] implementation for the negotiated version
/// that the caller can use to run the migration.
pub(crate) async fn initiate<'e>(
    log: &slog::Logger,
    migrate_info: &InstanceMigrateInitiateRequest,
    local_addr: SocketAddr,
) -> Result<impl DestinationProtocol<'e>, MigrateError> {
    let migration_id = migrate_info.migration_id;

    let log = log.new(slog::o!(
        "migration_id" => migration_id.to_string(),
        "migrate_role" => "destination",
        "migrate_src_addr" => migrate_info.src_addr
    ));

    info!(log, "negotiating migration as destination");

    // Build upgrade request to the source instance
    // (we do this by hand because it's hidden from the OpenAPI spec)
    // TODO(#165): https (wss)
    // TODO: We need to make sure the src_addr is a valid target
    let src_migrate_url = format!(
        "ws://{}/instance/migrate/{}/start",
        migrate_info.src_addr, migration_id,
    );
    info!(log, "Begin migration"; "src_migrate_url" => &src_migrate_url);
    let (mut conn, _) =
        tokio_tungstenite::connect_async(src_migrate_url).await?;

    let dst_protocols = super::protocol::make_protocol_offer();
    conn.send(tungstenite::Message::Text(dst_protocols)).await?;
    let selected = match conn.next().await {
        Some(Ok(tungstenite::Message::Text(selected_protocol))) => {
            info!(log, "source negotiated protocol {}", selected_protocol);
            match super::protocol::select_protocol_from_offer(
                &selected_protocol,
            ) {
                Ok(Some(selected)) => selected,
                Ok(None) => {
                    let offered = super::protocol::make_protocol_offer();
                    error!(log, "source selected protocol not on offer";
                           "offered" => &offered,
                           "selected" => &selected_protocol);

                    return Err(MigrateError::NoMatchingProtocol(
                        selected_protocol,
                        offered,
                    ));
                }
                Err(e) => {
                    error!(log, "source selected protocol failed to parse";
                           "selected" => &selected_protocol);

                    return Err(MigrateError::ProtocolParse(
                        selected_protocol,
                        e.to_string(),
                    ));
                }
            }
        }
        x => {
            conn.send(tungstenite::Message::Close(Some(CloseFrame {
                code: CloseCode::Protocol,
                reason: "did not respond to version handshake.".into(),
            })))
            .await?;
            error!(
                log,
                "source instance failed to negotiate protocol version: {:?}", x
            );
            return Err(MigrateError::Initiate);
        }
    };

    Ok(match selected {
        Protocol::RonV0 => RonV0::new(log, migration_id, conn, local_addr),
    })
}

/// A helper type for abstracting away type-level differences between the
/// different phases of instance ensure.
enum EnsureState<'a> {
    NotStarted(VmEnsureNotStarted<'a>),
    Active(VmEnsureActive<'a>),
}

impl<'a> EnsureState<'a> {
    /// Moves the state machine to the `Active` state by creating VM objects,
    /// preparing them for an incoming live migration, and activating the
    /// created objects.
    ///
    /// # Panics
    ///
    /// Panics if the state machine is not in the `NotStarted` state.
    async fn activate(self) -> anyhow::Result<Self> {
        let not_started = match self {
            EnsureState::NotStarted(vm) => vm,
            _ => unreachable!("can only activate a VM once"),
        };

        let mut objects_created = not_started.create_objects().await?;
        objects_created.prepare_for_migration().await;
        let active = objects_created.ensure_active().await;
        Ok(Self::Active(active))
    }

    /// Publishes the supplied migration state to the server's external state
    /// channel.
    fn update(&mut self, id: Uuid, state: MigrationState) {
        let publisher = match self {
            EnsureState::NotStarted(vm) => vm.state_publisher(),
            EnsureState::Active(vm) => vm.state_publisher(),
        };

        publisher.update(ExternalStateUpdate::Migration(
            MigrationStateUpdate { state, id, role: MigrateRole::Destination },
        ));
    }

    /// Notifies the instance ensure helper that migration has failed and that
    /// VM initialization should therefore also fail.
    async fn fail(self, reason: MigrateError) -> MigrateError {
        match self {
            EnsureState::NotStarted(vm) => {
                let reason = vm.fail(reason.into()).await;
                reason
                    .downcast::<MigrateError>()
                    .expect("original error was a MigrateError")
            }
            EnsureState::Active(vm) => {
                vm.fail().await;
                reason
            }
        }
    }

    /// If the state machine is in the `Active` state, locks the VM objects
    /// shared and returns the corresponding guard. Returns `None` otherwise.
    async fn vm_objects(&self) -> Option<VmObjectsShared> {
        let objects = match self {
            EnsureState::NotStarted(_) => return None,
            EnsureState::Active(vm) => vm.vm_objects(),
        };

        Some(objects.lock_shared().await)
    }
}

/// The runner for version 0 of the LM protocol, using RON encoding.
struct RonV0<'e, T: MigrateConn> {
    /// The ID for this migration.
    migration_id: Uuid,

    /// The logger for messages from this protocol.
    log: slog::Logger,

    /// The channel to use to send messages to the state worker coordinating
    /// this migration.
    conn: WebSocketStream<T>,

    /// Local propolis-server address
    /// (to inform the source-side where to redirect its clients)
    local_addr: SocketAddr,

    /// The current state of the overall instance-ensure operation. Wrapped in
    /// an `Option` to allow the various ensure state-types to be taken and
    /// replaced.
    ensure_state: Option<EnsureState<'e>>,
}

#[async_trait::async_trait]
impl<'e, T: MigrateConn + Sync> DestinationProtocol<'e> for RonV0<'e, T> {
    async fn run<'ensure>(
        mut self,
        ensure: VmEnsureNotStarted<'ensure>,
    ) -> Result<VmEnsureActive<'ensure>, MigrateError>
    where
        'e: 'ensure,
    {
        info!(self.log(), "entering destination migration task");

        let result = async {
            // Run the sync phase to ensure that the source's instance spec is
            // compatible with the spec supplied in the ensure parameters.
            if let Err(e) = self.run_sync_phase(ensure).await {
                self.update_state(MigrationState::Error);
                return Err(self.ensure_state.take().unwrap().fail(e).await);
            }

            // The sync phase succeeded, so it's OK to go ahead with creating
            // the objects in the target's instance spec.
            let new_state =
                self.ensure_state.take().unwrap().activate().await.map_err(
                    |e| {
                        MigrateError::TargetInstanceInitializationFailed(
                            e.to_string(),
                        )
                    },
                )?;

            self.ensure_state = Some(new_state);

            // Now that the VM's objects exist, run the rest of the protocol to
            // import state into them.
            if let Err(e) = self.run_import_phases().await {
                self.update_state(MigrationState::Error);
                return Err(self.ensure_state.take().unwrap().fail(e).await);
            }

            // Everything succeeded, so extract the active ensured VM to return
            // to the caller.
            let EnsureState::Active(vm) = self.ensure_state.take().unwrap()
            else {
                panic!("invalid ensure state at end of migration in");
            };

            Ok(vm)
        }
        .await;

        info!(self.log(), "migration destination task completed";
              "succeeded" => result.is_ok());

        match result {
            Ok(vm) => Ok(vm),
            Err(err) => {
                // We encountered an error, try to inform the remote before
                // bailing Note, we don't use `?` here as this is a best effort
                // and we don't want an error encountered during this send to
                // shadow the run error from the caller.
                if let Ok(e) = codec::Message::Error(err.clone()).try_into() {
                    let _ = self.conn.send(e).await;
                }
                Err(err)
            }
        }
    }
}

impl<'ensure, T: MigrateConn> RonV0<'ensure, T> {
    fn new(
        log: slog::Logger,
        migration_id: Uuid,
        conn: WebSocketStream<T>,
        local_addr: SocketAddr,
    ) -> Self {
        Self { log, migration_id, conn, local_addr, ensure_state: None }
    }

    fn log(&self) -> &slog::Logger {
        &self.log
    }

    fn update_state(&mut self, state: MigrationState) {
        self.ensure_state.as_mut().unwrap().update(self.migration_id, state);
    }

    async fn vm_objects(&self) -> VmObjectsShared {
        self.ensure_state.as_ref().unwrap().vm_objects().await.unwrap()
    }

    async fn run_sync_phase(
        &mut self,
        ensure: VmEnsureNotStarted<'ensure>,
    ) -> Result<(), MigrateError> {
        self.ensure_state = Some(EnsureState::NotStarted(ensure));
        self.run_phase(MigratePhase::MigrateSync).await?;
        Ok(())
    }

    async fn run_import_phases(&mut self) -> Result<(), MigrateError> {
        // The RAM transfer phase runs twice, once before the source pauses and
        // once after. There is no explicit pause phase on the destination,
        // though, so that step does not appear here even though there are
        // pre- and post-pause steps.
        self.run_phase(MigratePhase::RamPushPrePause).await?;
        self.run_phase(MigratePhase::RamPushPostPause).await?;

        // Import of the time data *must* be done before we import device
        // state: the proper functioning of device timers depends on an adjusted
        // boot_hrtime.
        self.run_phase(MigratePhase::TimeData).await?;
        self.run_phase(MigratePhase::DeviceState).await?;
        self.run_phase(MigratePhase::RamPull).await?;
        self.run_phase(MigratePhase::ServerState).await?;
        self.run_phase(MigratePhase::Finish).await?;

        Ok(())
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
            MigratePhase::TimeData => self.time_data().await,
            MigratePhase::RamPull => self.ram_pull().await,
            MigratePhase::ServerState => self.server_state().await,
            MigratePhase::Finish => self.finish().await,
        };

        probes::migrate_phase_end!(|| { step.to_string() });

        res
    }

    async fn sync(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Sync);
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

        let EnsureState::NotStarted(vm) = &self.ensure_state.as_ref().unwrap()
        else {
            panic!("activated VM before sync phase ran");
        };

        if let Err(e) = preamble.is_migration_compatible(vm.instance_spec()) {
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
                self.update_state(MigrationState::RamPush)
            }
            MigratePhase::RamPushPostPause => {
                self.update_state(MigrationState::RamPushDirty)
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
        self.update_state(MigrationState::Pause);
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
        self.update_state(MigrationState::Device);

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
            let vm_objects = self.vm_objects().await;
            let migrate_ctx =
                MigrateCtx { mem: &vm_objects.access_mem().unwrap() };
            for device in devices {
                info!(
                    self.log(),
                    "Applying state to device {}", device.instance_name
                );

                let target = vm_objects
                    .device_by_name(&device.instance_name)
                    .ok_or_else(|| {
                    MigrateError::UnknownDevice(device.instance_name.clone())
                })?;
                self.import_device(&target, &device, &migrate_ctx)?;
            }
        }

        self.send_msg(codec::Message::Okay).await
    }

    // Get the guest time data from the source, make updates to it based on the
    // new host, and write the data out to bhvye.
    async fn time_data(&mut self) -> Result<(), MigrateError> {
        // Read time data sent by the source and deserialize
        let raw: String = match self.read_msg().await? {
            codec::Message::Serialized(encoded) => encoded,
            msg => {
                error!(self.log(), "time data: unexpected message: {msg:?}");
                return Err(MigrateError::UnexpectedMessage);
            }
        };
        info!(self.log(), "VMM Time Data: {:?}", raw);
        let time_data_src: vmm::time::VmTimeData = ron::from_str(&raw)
            .map_err(|e| {
                MigrateError::TimeData(format!(
                    "VMM Time Data deserialization error: {}",
                    e
                ))
            })?;
        probes::migrate_time_data_before!(|| {
            (
                time_data_src.guest_freq,
                time_data_src.guest_tsc,
                time_data_src.boot_hrtime,
            )
        });

        // Take a snapshot of the host hrtime/wall clock time, then adjust
        // time data appropriately.
        let vmm_hdl = &self.vm_objects().await.vmm_hdl().clone();

        let (dst_hrt, dst_wc) = vmm::time::host_time_snapshot(vmm_hdl)
            .map_err(|e| {
                MigrateError::TimeData(format!(
                    "could not read host time: {}",
                    e
                ))
            })?;
        let (time_data_dst, adjust) =
            vmm::time::adjust_time_data(time_data_src, dst_hrt, dst_wc)
                .map_err(|e| {
                    MigrateError::TimeData(format!(
                        "could not adjust VMM Time Data: {}",
                        e
                    ))
                })?;

        // In case import fails, log adjustments made to time data and fire
        // dtrace probe first
        if adjust.migrate_delta_negative {
            warn!(
                self.log(),
                "Found negative wall clock delta between target import \
                and source export:\n\
                - source wall clock time: {:?}\n\
                - target wall clock time: {:?}\n",
                time_data_src.wall_clock(),
                dst_wc
            );
        }
        info!(
            self.log(),
            "Time data adjustments:\n\
            - guest TSC freq: {} Hz = {} GHz\n\
            - guest uptime ns: {:?}\n\
            - migration time delta: {:?}\n\
            - guest_tsc adjustment = {} + {} = {}\n\
            - boot_hrtime adjustment = {} ---> {} - {} = {}\n\
            - dest highres clock time: {}\n\
            - dest wall clock time: {:?}",
            time_data_dst.guest_freq,
            time_data_dst.guest_freq as f64 / vmm::time::NS_PER_SEC as f64,
            adjust.guest_uptime_ns,
            adjust.migrate_delta,
            time_data_src.guest_tsc,
            adjust.guest_tsc_delta,
            time_data_dst.guest_tsc,
            time_data_src.boot_hrtime,
            dst_hrt,
            adjust.boot_hrtime_delta,
            time_data_dst.boot_hrtime,
            dst_hrt,
            dst_wc
        );
        probes::migrate_time_data_after!(|| {
            (
                time_data_dst.guest_freq,
                time_data_dst.guest_tsc,
                time_data_dst.boot_hrtime,
                adjust.guest_uptime_ns,
                adjust.migrate_delta.as_nanos() as u64,
                adjust.migrate_delta_negative,
            )
        });

        // Import the adjusted time data
        vmm::time::import_time_data(vmm_hdl, time_data_dst).map_err(|e| {
            MigrateError::TimeData(format!("VMM Time Data import error: {}", e))
        })?;

        self.send_msg(codec::Message::Okay).await
    }

    fn import_device(
        &self,
        target: &Arc<dyn Lifecycle>,
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
        self.update_state(MigrationState::RamPull);
        self.send_msg(codec::Message::MemQuery(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_pull: got end {:?}", m);
        self.send_msg(codec::Message::MemDone).await
    }

    async fn server_state(&mut self) -> Result<(), MigrateError> {
        self.update_state(MigrationState::Server);
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

        self.vm_objects()
            .await
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

        // The source has acknowledged the migration is complete, so it's safe
        // to declare victory publicly.
        self.update_state(MigrationState::Finish);
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
                    error!(
                        self.log(),
                        "migration failed due to error from source: {err}"
                    );
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
        let objects = self.vm_objects().await;
        let memctx = objects.access_mem().unwrap();
        let len = buf.len();
        memctx.write_from(addr, buf, len);
        Ok(())
    }
}
