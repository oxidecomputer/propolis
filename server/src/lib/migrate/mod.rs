use std::sync::Arc;

use dropshot::{HttpError, RequestContext};
use hyper::{header, Body, Method, Response, StatusCode};
use propolis_client::api::{self, MigrationState};
use serde::{Deserialize, Serialize};
use slog::{error, info, o};
use thiserror::Error;
use tokio::{sync::RwLock, task::JoinHandle};
use uuid::Uuid;

use crate::server::Context;

mod codec;
mod destination;
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

pub struct MigrateContext {
    migration_id: Uuid,
    state: RwLock<MigrationState>,
}

impl MigrateContext {
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
#[derive(Error, Debug, Deserialize, PartialEq, Serialize)]
pub enum MigrateError {
    #[error("HTTP error: {0}")]
    Http(String),

    #[error("couldn't establish migration connection to source instance")]
    Initiate,

    #[error("the source ({0}) and destination ({1}) instances are incompatible for migration")]
    Incompatible(String, String),

    #[error("expected connection upgrade")]
    UpgradeExpected,

    #[error("source instance is not initialized")]
    SourceNotInitialized,

    #[error("unexpected Uuid")]
    UuidMismatch,

    #[error("a migration from the current instance is already in progress")]
    MigrationAlreadyInProgress,

    #[error("no migration is currently in progress")]
    NoMigrationInProgress,

    #[error("protocol error")]
    // TODO: just for testing rn
    Protocol,

    #[error("encoding error")]
    Encoding,
}

impl From<hyper::Error> for MigrateError {
    fn from(err: hyper::Error) -> MigrateError {
        MigrateError::Http(err.to_string())
    }
}

impl MigrateError {
    fn incompatible(src: &str, dst: &str) -> MigrateError {
        MigrateError::Incompatible(src.to_string(), dst.to_string())
    }
}

impl Into<HttpError> for MigrateError {
    fn into(self) -> HttpError {
        let msg = format!("migration failed: {}", self);
        match &self {
            MigrateError::Http(_)
            | MigrateError::Initiate
            | MigrateError::Incompatible(_, _)
            | MigrateError::SourceNotInitialized => {
                HttpError::for_internal_error(msg)
            }
            MigrateError::MigrationAlreadyInProgress
            | MigrateError::NoMigrationInProgress
            | MigrateError::Protocol
            | MigrateError::Encoding
            | MigrateError::UuidMismatch
            | MigrateError::UpgradeExpected => {
                HttpError::for_bad_request(None, msg)
            }
        }
    }
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
        context.as_mut().ok_or_else(|| MigrateError::SourceNotInitialized)?;

    if instance_id != context.properties.id {
        return Err(MigrateError::UuidMismatch);
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

    let upgrade = hyper::upgrade::on(request);
    let instance = context.instance.clone();

    let migrate_context = Arc::new(MigrateContext {
        migration_id,
        state: RwLock::new(MigrationState::Sync),
    });

    // We've successfully negotiated a migration protocol w/ the destination.
    // Now, we spawn a new task to handle the actual migration over the upgraded socket
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
        // TODO: wrap in a tokio codec::Framed or such
        if let Err(e) = source::migrate(mctx, instance, conn, log.clone()).await
        {
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
    _instance_id: Uuid,
    migrate_info: api::InstanceMigrateInitiateRequest,
) -> Result<api::InstanceMigrateInitiateResponse, MigrateError> {
    // Create a new UUID to refer to this migration across both the source
    // and destination instances
    let migration_id = Uuid::new_v4();

    // Create a new log context for the migration
    let log = rqctx.log.new(o!(
        "migration_id" => migration_id.to_string(),
        "migrate_role" => "destination",
        "migrate_src_addr" => migrate_info.src_addr.clone()
    ));
    info!(log, "Migration Destination");

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

    let migrate_context = Arc::new(MigrateContext {
        migration_id,
        state: RwLock::new(MigrationState::Sync),
    });

    // We've successfully negotiated a migration protocol w/ the source.
    // Now, we spawn a new task to handle the actual migration over the upgraded socket
    let mctx = migrate_context.clone();
    let task_rqctx = rqctx.clone();
    let task = tokio::spawn(async move {
        if let Err(e) =
            destination::migrate(task_rqctx, mctx, conn, log.clone()).await
        {
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
