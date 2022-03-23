use std::sync::Arc;

use bit_field::BitField;
use dropshot::{HttpError, RequestContext};
use hyper::{header, Body, Method, Response, StatusCode};
use propolis::{
    dispatch::AsyncCtx,
    instance::{Instance, MigratePhase, MigrateRole, State, TransitionError},
    migrate::MigrateStateError,
};
use propolis_client::api::{self, MigrationState};
use serde::{Deserialize, Serialize};
use slog::{error, info, o};
use thiserror::Error;
use tokio::{sync::RwLock, task::JoinHandle};
use uuid::Uuid;

use crate::server::Context;

mod codec;
mod destination;
mod memx;
mod preamble;
mod source;

/// Our migration protocol version
const MIGRATION_PROTOCOL_VERION: usize = 0;

/// Our migration protocol encoding
const MIGRATION_PROTOCOL_ENCODING: ProtocolEncoding = ProtocolEncoding::Ron;

/// The concatenated migration protocol-encoding-version string
const MIGRATION_PROTOCOL_STR: &'static str = const_format::concatcp!(
    "propolis-migrate-",
    encoding_str(MIGRATION_PROTOCOL_ENCODING),
    "/",
    MIGRATION_PROTOCOL_VERION
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

/// Context created as part of a migration.
pub struct MigrateContext {
    /// The external ID used to identify a migration across both the source and destination.
    migration_id: Uuid,

    /// The current state of the migration process on this Instance.
    state: RwLock<MigrationState>,

    /// A handle to the underlying propolis [`Instance`].
    instance: Arc<Instance>,

    /// Async descriptor context for the migrate task to access machine state in async context.
    async_ctx: AsyncCtx,

    /// Logger for migration created from initial migration request.
    log: slog::Logger,
}

impl MigrateContext {
    fn new(
        migration_id: Uuid,
        instance: Arc<Instance>,
        log: slog::Logger,
    ) -> MigrateContext {
        MigrateContext {
            migration_id,
            state: RwLock::new(MigrationState::Sync),
            async_ctx: instance.async_ctx(),
            instance,
            log,
        }
    }

    async fn get_state(&self) -> MigrationState {
        let state = self.state.read().await;
        *state
    }

    async fn set_state(&self, new: MigrationState) {
        let mut state = self.state.write().await;
        *state = new;
    }
}

pub struct MigrateTask {
    #[allow(dead_code)]
    task: JoinHandle<()>,
    context: Arc<MigrateContext>,
}

/// Errors which may occur during the course of a migration
#[derive(Clone, Debug, Error, Deserialize, PartialEq, Serialize)]
pub enum MigrateError {
    /// An error as a result of some HTTP operation (i.e. trying to establish
    /// the websocket connection between the source and destination)
    #[error("HTTP error: {0}")]
    Http(String),

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
    #[error("{0} migration instance encountered error: {1}")]
    RemoteError(MigrateRole, String),
}

impl MigrateError {
    fn incompatible(src: &str, dst: &str) -> MigrateError {
        MigrateError::Incompatible(src.to_string(), dst.to_string())
    }
}

impl From<hyper::Error> for MigrateError {
    fn from(err: hyper::Error) -> MigrateError {
        MigrateError::Http(err.to_string())
    }
}

impl From<TransitionError> for MigrateError {
    fn from(err: TransitionError) -> Self {
        match err {
            TransitionError::ResetWhileHalted
            | TransitionError::InvalidTarget { .. }
            | TransitionError::Terminal => MigrateError::InvalidInstanceState,
            TransitionError::MigrationAlreadyInProgress => {
                MigrateError::MigrationAlreadyInProgress
            }
        }
    }
}

impl From<codec::ProtocolError> for MigrateError {
    fn from(err: codec::ProtocolError) -> Self {
        MigrateError::Codec(err.to_string())
    }
}

impl Into<HttpError> for MigrateError {
    fn into(self) -> HttpError {
        let msg = format!("migration failed: {}", self);
        match &self {
            MigrateError::Http(_)
            | MigrateError::Initiate
            | MigrateError::Incompatible(_, _)
            | MigrateError::InstanceNotInitialized
            | MigrateError::InvalidInstanceState
            | MigrateError::Codec(_)
            | MigrateError::UnexpectedMessage
            | MigrateError::SourcePause
            | MigrateError::Phase
            | MigrateError::DeviceState(_)
            | MigrateError::RemoteError(_, _) => {
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
///This will attempt to upgrade the given HTTP request to a `propolis-migrate`
/// connection and begin the migration in a separate task.
pub async fn source_start(
    rqctx: Arc<RequestContext<Context>>,
    instance_id: Uuid,
    migration_id: Uuid,
) -> Result<Response<Body>, MigrateError> {
    // Create a new log context for the migration
    let log = rqctx.log.new(o!(
        "migration_id" => migration_id.to_string(),
        "migrate_role" => "source"
    ));
    info!(log, "Migration Source");

    let mut context = rqctx.context().context.lock().await;
    let context =
        context.as_mut().ok_or_else(|| MigrateError::InstanceNotInitialized)?;

    if instance_id != context.properties.id {
        return Err(MigrateError::UuidMismatch);
    }

    // Bail if the instance hasn't been preset to Migrate Start state.
    if !matches!(
        context.instance.current_state(),
        State::Migrate(MigrateRole::Source, MigratePhase::Start)
    ) {
        return Err(MigrateError::InvalidInstanceState);
    }

    // Bail if there's already one in progress
    // TODO: Should we just instead hold the context lock during the whole process?
    let mut migrate_task = rqctx.context().migrate_task.lock().await;
    if migrate_task.is_some() {
        return Err(MigrateError::MigrationAlreadyInProgress);
    }

    let request = &mut *rqctx.request.lock().await;

    // Check this is a valid migration request
    if !request
        .headers()
        .get(header::CONNECTION)
        .and_then(|hv| hv.to_str().ok())
        .map(|hv| hv.eq_ignore_ascii_case("upgrade"))
        .unwrap_or(false)
    {
        return Err(MigrateError::UpgradeExpected);
    }

    let src_protocol = MIGRATION_PROTOCOL_STR;
    let dst_protocol = request
        .headers()
        .get(header::UPGRADE)
        .ok_or_else(|| MigrateError::UpgradeExpected)
        .map(|hv| hv.to_str().ok())?
        .ok_or_else(|| MigrateError::incompatible(src_protocol, "<unknown>"))?;

    // TODO: improve "negotiation"
    if !dst_protocol.eq_ignore_ascii_case(MIGRATION_PROTOCOL_STR) {
        error!(
            log,
            "incompatible with destination instance provided protocol ({})",
            dst_protocol
        );
        return Err(MigrateError::incompatible(src_protocol, dst_protocol));
    }

    // Grab the future for plucking out the upgraded socket
    let upgrade = hyper::upgrade::on(request);

    // We've successfully negotiated a migration protocol w/ the destination.
    // Now, we spawn a new task to handle the actual migration over the upgraded socket
    let migrate_context = Arc::new(MigrateContext::new(
        migration_id,
        context.instance.clone(),
        log.clone(),
    ));
    let mctx = migrate_context.clone();
    let task = tokio::spawn(async move {
        // We have to await on the HTTP upgrade future in a new
        // task because it won't complete until the response is
        // sent, i.e., the outer function returns the 101 Resposne.
        let conn = match upgrade.await {
            Ok(upgraded) => upgraded,
            Err(e) => {
                error!(log, "Migrate Task Failed: {}", e);
                return;
            }
        };

        // Good to go, ready to migrate to the dest via `conn`
        if let Err(e) = source::migrate(mctx, conn).await {
            error!(log, "Migrate Task Failed: {}", e);
            return;
        }
    });

    // Save active migration task handle
    *migrate_task = Some(MigrateTask { task, context: migrate_context });

    // Complete the request with an HTTP 101 response so that the
    // destination knows we're ready
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, src_protocol)
        .body(Body::empty())
        .unwrap())
}

/// Initiate a migration to the given source instance.
///
/// This will attempt to send an HTTP request, along with a request to upgrade
/// it to a `propolis-migrate` connection, to the given source instance. Once
/// we've successfully established the connection, we can begin the migration
/// process (destination-side).
pub async fn dest_initiate(
    rqctx: Arc<RequestContext<Context>>,
    instance_id: Uuid,
    migrate_info: api::InstanceMigrateInitiateRequest,
) -> Result<api::InstanceMigrateInitiateResponse, MigrateError> {
    let migration_id = migrate_info.migration_id;

    // Create a new log context for the migration
    let log = rqctx.log.new(o!(
        "migration_id" => migration_id.to_string(),
        "migrate_role" => "destination",
        "migrate_src_addr" => migrate_info.src_addr.clone()
    ));
    info!(log, "Migration Destination");

    let mut context = rqctx.context().context.lock().await;
    let context =
        context.as_mut().ok_or_else(|| MigrateError::InstanceNotInitialized)?;

    if instance_id != context.properties.id {
        return Err(MigrateError::UuidMismatch);
    }

    let mut migrate_task = rqctx.context().migrate_task.lock().await;

    // This should be a fresh propolis-server
    assert!(migrate_task.is_none());

    // TODO: https
    // TODO: We need to make sure the src_addr is a valid target
    let src_migrate_url = format!(
        "http://{}/instances/{}/migrate/start",
        migrate_info.src_addr, migrate_info.src_uuid
    );
    info!(log, "Begin migration"; "src_migrate_url" => &src_migrate_url);

    let body = Body::from(
        serde_json::to_string(&api::InstanceMigrateStartRequest {
            migration_id,
        })
        .unwrap(),
    );

    // Build upgrade request to the source instance
    let dst_protocol = MIGRATION_PROTOCOL_STR;
    let req = hyper::Request::builder()
        .method(Method::PUT)
        .uri(src_migrate_url)
        .header(header::CONNECTION, "upgrade")
        // TODO: move to constant
        .header(header::UPGRADE, dst_protocol)
        .body(body)
        .unwrap();

    // Kick off the request
    let res = hyper::Client::new().request(req).await?;
    if res.status() != StatusCode::SWITCHING_PROTOCOLS {
        error!(
            log,
            "source instance failed to switch protocols: {}",
            res.status()
        );
        return Err(MigrateError::Initiate);
    }
    let src_protocol = res
        .headers()
        .get(header::UPGRADE)
        .ok_or_else(|| MigrateError::UpgradeExpected)
        .map(|hv| hv.to_str().ok())?
        .ok_or_else(|| MigrateError::incompatible("<unknown>", dst_protocol))?;

    // TODO: improve "negotiation"
    if !src_protocol.eq_ignore_ascii_case(dst_protocol) {
        error!(
            log,
            "incompatible with source instance provided protocol ({})",
            src_protocol
        );
        return Err(MigrateError::incompatible(src_protocol, dst_protocol));
    }

    // Now co-opt the socket for the migration protocol
    let conn = hyper::upgrade::on(res).await?;

    // We've successfully negotiated a migration protocol w/ the source.
    // Now, we spawn a new task to handle the actual migration over the upgraded socket
    let migrate_context = Arc::new(MigrateContext::new(
        migration_id,
        context.instance.clone(),
        log.clone(),
    ));
    let mctx = migrate_context.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = destination::migrate(mctx, conn).await {
            error!(log, "Migrate Task Failed: {}", e);
            return;
        }
    });

    // Save active migration task handle
    *migrate_task = Some(MigrateTask { task, context: migrate_context });

    Ok(api::InstanceMigrateInitiateResponse { migration_id })
}

/// Return the current status of an ongoing migration
pub async fn migrate_status(
    rqctx: Arc<RequestContext<Context>>,
    migration_id: Uuid,
) -> Result<api::InstanceMigrateStatusResponse, MigrateError> {
    let migrate_task = rqctx.context().migrate_task.lock().await;
    let migrate_task = migrate_task
        .as_ref()
        .ok_or_else(|| MigrateError::NoMigrationInProgress)?;

    if migration_id != migrate_task.context.migration_id {
        return Err(MigrateError::UuidMismatch);
    }

    Ok(api::InstanceMigrateStatusResponse {
        state: migrate_task.context.get_state().await,
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
