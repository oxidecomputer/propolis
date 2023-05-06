use std::sync::Arc;

use bit_field::BitField;
use dropshot::{HttpError, RequestContext};
use futures::{SinkExt, StreamExt};
use propolis::migrate::MigrateStateError;
use propolis_client::handmade::api::{self, MigrationState};
use serde::{Deserialize, Serialize};
use slog::{error, info, o};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{tungstenite, WebSocketStream};
use uuid::Uuid;

use crate::{
    server::{DropshotEndpointContext, VmControllerState},
    vm::{VmController, VmControllerError},
};

mod codec;
pub mod destination;
mod memx;
mod preamble;
pub mod source;

/// Our migration protocol version
const MIGRATION_PROTOCOL_VERSION: usize = 0;

/// Our migration protocol encoding
const MIGRATION_PROTOCOL_ENCODING: ProtocolEncoding = ProtocolEncoding::Ron;

/// The concatenated migration protocol-encoding-version string
const MIGRATION_PROTOCOL_STR: &str = const_format::concatcp!(
    "propolis-migrate-",
    encoding_str(MIGRATION_PROTOCOL_ENCODING),
    "/",
    MIGRATION_PROTOCOL_VERSION
);

/// Supported encoding formats
enum ProtocolEncoding {
    Ron,
}

/// Small helper function to stringify ProtocolEncoding
const fn encoding_str(e: ProtocolEncoding) -> &'static str {
    match e {
        ProtocolEncoding::Ron => "ron",
    }
}

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

    /// The source and destination instances are not compatible
    #[error("the source ({0}) and destination ({1}) instances are incompatible for migration")]
    Incompatible(String, String),

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

    /// Failed to export/import device state for migration
    #[error("failed to migrate device state: {0}")]
    DeviceState(#[from] MigrateStateError),

    /// The destination instance doesn't recognize the received device
    #[error("received device state for unknown device ({0})")]
    UnknownDevice(String),

    /// The other end of the migration ran into an error
    #[error("{0:?} migration instance encountered error: {1}")]
    RemoteError(MigrateRole, String),
}

impl MigrateError {
    fn incompatible(src: &str, dst: &str) -> MigrateError {
        MigrateError::Incompatible(src.to_string(), dst.to_string())
    }
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

impl From<VmControllerError> for MigrateError {
    fn from(err: VmControllerError) -> Self {
        match err {
            VmControllerError::AlreadyMigrationSource => {
                MigrateError::MigrationAlreadyInProgress
            }
            _ => MigrateError::StateMachine(err.to_string()),
        }
    }
}

impl From<MigrateError> for HttpError {
    fn from(err: MigrateError) -> Self {
        let msg = format!("migration failed: {}", err);
        match &err {
            MigrateError::Websocket(_)
            | MigrateError::Initiate
            | MigrateError::Incompatible(_, _)
            | MigrateError::InstanceNotInitialized
            | MigrateError::InvalidInstanceState
            | MigrateError::Codec(_)
            | MigrateError::UnexpectedMessage
            | MigrateError::SourcePause
            | MigrateError::Phase
            | MigrateError::DeviceState(_)
            | MigrateError::RemoteError(_, _)
            | MigrateError::StateMachine(_) => {
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
    instance_name: String,

    /// The (Ron) serialized device state.
    /// See `Migrate::export`.
    payload: String,
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

    let controller = tokio::sync::MutexGuard::try_map(
        rqctx.context().services.vm.lock().await,
        VmControllerState::as_controller,
    )
    .map_err(|_| MigrateError::InstanceNotInitialized)?;

    let src_protocol = MIGRATION_PROTOCOL_STR;

    match conn.next().await {
        Some(Ok(tungstenite::Message::Text(dst_protocol))) => {
            // TODO: improve "negotiation"
            if !dst_protocol.eq_ignore_ascii_case(src_protocol) {
                error!(
                    log,
                    "incompatible with destination instance provided protocol ({})",
                    dst_protocol
                );
                return Err(MigrateError::incompatible(
                    src_protocol,
                    &dst_protocol,
                ));
            }

            // Complete the negotiation with our own version string so that the
            // destination knows we're ready
            conn.send(tungstenite::Message::Text(src_protocol.to_string()))
                .await?;
        }
        x => {
            conn.send(tungstenite::Message::Close(Some(CloseFrame {
                code: CloseCode::Protocol,
                reason: "did not begin with version handshake.".into(),
            })))
            .await?;
            error!(log, "destination side did not begin migration version handshake: {:?}", x);
            return Err(MigrateError::Initiate);
        }
    }

    controller.request_migration_from(migration_id, conn)?;
    Ok(())
}

/// Initiate a migration to the given source instance.
///
/// This will attempt to open a websocket to the given source instance and
/// check that the migrate protocol version is compatible ("equal" presently).
/// Once we've successfully established the connection, we can begin the
/// migration process (destination-side).
pub(crate) async fn dest_initiate(
    rqctx: &RequestContext<Arc<DropshotEndpointContext>>,
    controller: Arc<VmController>,
    migrate_info: api::InstanceMigrateInitiateRequest,
) -> Result<api::InstanceMigrateInitiateResponse, MigrateError> {
    let migration_id = migrate_info.migration_id;

    // Create a new log context for the migration
    let log = rqctx.log.new(o!(
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

    let dst_protocol = MIGRATION_PROTOCOL_STR;
    conn.send(tungstenite::Message::Text(dst_protocol.to_string())).await?;
    match conn.next().await {
        Some(Ok(tungstenite::Message::Text(src_protocol))) => {
            // TODO: improve "negotiation"
            if !src_protocol.eq_ignore_ascii_case(dst_protocol) {
                error!(
                    log,
                    "incompatible with source's provided protocol ({})",
                    src_protocol
                );
                return Err(MigrateError::incompatible(
                    &src_protocol,
                    dst_protocol,
                ));
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
    }
    let local_addr = rqctx.server.local_addr;
    tokio::runtime::Handle::current()
        .spawn_blocking(move || -> Result<(), MigrateError> {
            // Now start using the websocket for the migration protocol
            controller.request_migration_into(
                migration_id,
                conn,
                local_addr,
            )?;
            Ok(())
        })
        .await
        .unwrap()?;

    Ok(api::InstanceMigrateInitiateResponse { migration_id })
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
    fn migrate_xfer_ram_page(addr: u64, size: u64, paused: u8) {}
}
