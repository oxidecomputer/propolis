// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::net::SocketAddr;
use std::sync::Arc;

use bit_field::BitField;
use dropshot::{HttpError, RequestContext};
use futures::{SinkExt, StreamExt};
use propolis::migrate::MigrateStateError;
use propolis_api_types::{self as api, MigrationState};
use serde::{Deserialize, Serialize};
use slog::{error, info, o};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{tungstenite, WebSocketStream};
use uuid::Uuid;

use crate::server::DropshotEndpointContext;

mod codec;
pub mod destination;
mod memx;
mod preamble;
pub mod protocol;
pub mod source;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MigrateRole {
    Source,
    Destination,
}

// N.B. Keep in sync with scripts/live-migration-times.d.
#[derive(Debug)]
enum MigratePhase {
    MigrateSync,
    Pause,
    RamPushPrePause,
    RamPushPostPause,
    TimeData,
    DeviceState,
    RamPull,
    ServerState,
    Finish,
}

impl std::fmt::Display for MigratePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MigratePhase::MigrateSync => "Sync",
            MigratePhase::Pause => "Pause",
            MigratePhase::RamPushPrePause => "RamPushPrePause",
            MigratePhase::RamPushPostPause => "RamPushPostPause",
            MigratePhase::TimeData => "TimeData",
            MigratePhase::DeviceState => "DeviceState",
            MigratePhase::RamPull => "RamPull",
            MigratePhase::ServerState => "ServerState",
            MigratePhase::Finish => "Finish",
        };

        write!(f, "{}", s)
    }
}

/// Errors which may occur during the course of a migration
#[derive(Clone, Debug, Error, Deserialize, PartialEq, Serialize)]
pub enum MigrateError {
    /// An error as a result of some Websocket operation (i.e. establishing
    /// or maintaining the connection between the source and destination)
    #[error("Websocket error: {0}")]
    Websocket(String),

    /// Failed to initiate the migration protocol
    #[error("couldn't establish migration connection to source instance")]
    Initiate,

    #[error("failed to parse the offered protocol list ({0}): {1}")]
    ProtocolParse(String, String),

    /// The source and destination instances are not compatible
    #[error("the source ({0}) and destination ({1}) instances have no common protocol")]
    NoMatchingProtocol(String, String),

    /// Incomplete WebSocket upgrade request
    #[error("expected connection upgrade")]
    UpgradeExpected,

    /// Attempted to migrate an uninitialized instance
    #[error("instance is not initialized")]
    InstanceNotInitialized,

    /// The given UUID does not match the existing instance/migration UUID
    #[error("unexpected Uuid")]
    UuidMismatch,

    /// A different migration already in progress
    #[error("a migration from the current instance is already in progress")]
    MigrationAlreadyInProgress,

    /// Migration state was requested with no migration in process
    #[error("no migration is currently in progress")]
    NoMigrationInProgress,

    /// A VM controller function returned an error
    #[error("VM state machine error: {0}")]
    StateMachine(String),

    /// Encountered an error as part of encoding/decoding migration messages
    #[error("codec error: {0}")]
    Codec(String),

    /// The instance is in an invalid state for the current operation
    #[error("encountered invalid instance state")]
    InvalidInstanceState,

    /// Received a message out of order
    #[error("received unexpected migration message")]
    UnexpectedMessage,

    /// Failed to pause the source instance's devices or tasks
    #[error("failed to pause source instance")]
    SourcePause,

    /// Phase error
    #[error("received out-of-phase message")]
    Phase,

    /// Failed to export/import time data state
    #[error("failed to migrate VMM time data: {0}")]
    TimeData(String),

    /// Failed to export/import device state for migration
    #[error("failed to migrate device state: {0}")]
    DeviceState(String),

    /// The destination instance doesn't recognize the received device
    #[error("received device state for unknown device ({0})")]
    UnknownDevice(String),

    /// The other end of the migration ran into an error
    #[error("{0:?} migration instance encountered error: {1}")]
    RemoteError(MigrateRole, String),

    /// Sending/receiving from the VM state driver command/response channels
    /// returned an error.
    #[error("unable to communiciate with VM state driver")]
    StateDriverChannelClosed,

    #[error("request to VM state driver returned failure")]
    StateDriverResponseFailed,
}

impl From<tokio_tungstenite::tungstenite::Error> for MigrateError {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> MigrateError {
        MigrateError::Websocket(err.to_string())
    }
}

impl From<codec::ProtocolError> for MigrateError {
    fn from(err: codec::ProtocolError) -> Self {
        MigrateError::Codec(err.to_string())
    }
}

impl From<MigrateStateError> for MigrateError {
    fn from(value: MigrateStateError) -> Self {
        Self::DeviceState(value.to_string())
    }
}

impl From<MigrateError> for HttpError {
    fn from(err: MigrateError) -> Self {
        let msg = format!("migration failed: {}", err);
        match &err {
            MigrateError::Websocket(_)
            | MigrateError::Initiate
            | MigrateError::ProtocolParse(_, _)
            | MigrateError::NoMatchingProtocol(_, _)
            | MigrateError::InstanceNotInitialized
            | MigrateError::InvalidInstanceState
            | MigrateError::Codec(_)
            | MigrateError::UnexpectedMessage
            | MigrateError::SourcePause
            | MigrateError::Phase
            | MigrateError::TimeData(_)
            | MigrateError::DeviceState(_)
            | MigrateError::RemoteError(_, _)
            | MigrateError::StateMachine(_)
            | MigrateError::StateDriverChannelClosed
            | MigrateError::StateDriverResponseFailed => {
                HttpError::for_internal_error(msg)
            }
            MigrateError::MigrationAlreadyInProgress
            | MigrateError::NoMigrationInProgress
            | MigrateError::UuidMismatch
            | MigrateError::UpgradeExpected
            | MigrateError::UnknownDevice(_) => {
                HttpError::for_bad_request(None, msg)
            }
        }
    }
}

/// Serialized device state sent during migration.
#[derive(Debug, Deserialize, Serialize)]
struct Device {
    /// The unique name identifying the device in the instance inventory.
    pub instance_name: String,

    /// Device state data
    pub payload: Vec<DevicePayload>,
}
#[derive(Debug, Deserialize, Serialize)]
struct DevicePayload {
    /// Payload schema type
    pub kind: String,

    /// Payload schema version
    pub version: u32,

    /// Serialized device state.
    pub data: String,
}

/// Begin the migration process (source-side).
///
/// This will check protocol version and then begin the migration in a separate task.
pub async fn source_start<
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
>(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    migration_id: Uuid,
    mut conn: WebSocketStream<T>,
) -> Result<(), MigrateError> {
    // Create a new log context for the migration
    let log = rqctx.log.new(o!(
        "migration_id" => migration_id.to_string(),
        "migrate_role" => "source"
    ));
    info!(log, "Migration Source");

    let active_vm = rqctx
        .context()
        .vm
        .active_vm()
        .ok_or_else(|| MigrateError::InstanceNotInitialized)?
        .clone();

    let selected = match conn.next().await {
        Some(Ok(tungstenite::Message::Text(dst_protocols))) => {
            info!(log, "destination offered protocols: {}", dst_protocols);
            match protocol::select_protocol_from_offer(&dst_protocols) {
                Ok(Some(selected)) => {
                    info!(log, "selected protocol {:?}", selected);
                    conn.send(tungstenite::Message::Text(
                        selected.offer_string(),
                    ))
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
            }
        }
        x => {
            conn.send(tungstenite::Message::Close(Some(CloseFrame {
                code: CloseCode::Protocol,
                reason: "did not begin with version handshake.".into(),
            })))
            .await?;
            error!(
                log,
                "destination side did not begin migration version handshake: \
                 {:?}",
                x
            );
            return Err(MigrateError::Initiate);
        }
    };

    todo!("gjc"); // need a method on ActiveVm for this
                  // controller.request_migration_from(migration_id, conn, selected)?;
    Ok(())
}

pub(crate) struct DestinationContext<
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
> {
    pub migration_id: Uuid,
    pub conn: WebSocketStream<T>,
    pub local_addr: SocketAddr,
    pub protocol: crate::migrate::protocol::Protocol,
}

/// Initiate a migration to the given source instance.
///
/// This will attempt to open a websocket to the given source instance and
/// check that the migrate protocol version is compatible ("equal" presently).
/// Once we've successfully established the connection, we can begin the
/// migration process (destination-side).
pub(crate) async fn dest_initiate(
    log: &slog::Logger,
    migrate_info: api::InstanceMigrateInitiateRequest,
    local_server_addr: SocketAddr,
) -> Result<
    DestinationContext<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    MigrateError,
> {
    let migration_id = migrate_info.migration_id;

    // Create a new log context for the migration
    let log = log.new(o!(
        "migration_id" => migration_id.to_string(),
        "migrate_role" => "destination",
        "migrate_src_addr" => migrate_info.src_addr
    ));
    info!(log, "Migration Destination");

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

    let dst_protocols = protocol::make_protocol_offer();
    conn.send(tungstenite::Message::Text(dst_protocols)).await?;
    let selected = match conn.next().await {
        Some(Ok(tungstenite::Message::Text(selected_protocol))) => {
            info!(log, "source negotiated protocol {}", selected_protocol);
            match protocol::select_protocol_from_offer(&selected_protocol) {
                Ok(Some(selected)) => selected,
                Ok(None) => {
                    let offered = protocol::make_protocol_offer();
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

    Ok(DestinationContext {
        migration_id,
        conn,
        local_addr: local_server_addr,
        protocol: selected,
    })
}

// We should probably turn this into some kind of ValidatedBitmap
// data structure, so that we're only parsing it once.
struct PageIter<'a> {
    start: u64,
    current: u64,
    end: u64,
    bits: &'a [u8],
}

impl<'a> PageIter<'a> {
    pub fn new(start: u64, end: u64, bits: &'a [u8]) -> PageIter<'a> {
        let current = start;
        PageIter { start, current, end, bits }
    }
}

impl<'a> Iterator for PageIter<'a> {
    type Item = u64;
    fn next(&mut self) -> Option<Self::Item> {
        while self.current < self.end {
            let addr = self.current;
            self.current += 4096;
            let page_offset = ((addr - self.start) / 4096) as usize;
            let b = self.bits[page_offset / 8];
            if b.get_bit(page_offset % 8) {
                return Some(addr);
            }
        }
        None
    }
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn migrate_phase_begin(step_desc: &str) {}
    fn migrate_phase_end(step_desc: &str) {}
    fn migrate_xfer_ram_region(pages: u64, size: u64, paused: u8) {}
    fn migrate_xfer_ram_page(addr: u64, size: u64) {}
    fn migrate_time_data_before(
        src_guest_freq: u64,
        src_guest_tsc: u64,
        src_boot_hrtime: i64,
    ) {
    }
    fn migrate_time_data_after(
        dst_guest_freq: u64,
        dst_guest_tsc: u64,
        dst_boot_hrtime: i64,
        guest_uptime: u64,
        migrate_delta_ns: u64,
        migrate_delta_negative: bool,
    ) {
    }
}
