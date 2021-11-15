use std::sync::Arc;

use dropshot::{HttpError, HttpResponseOk, RequestContext};
use hyper::{Body, Method, Response, StatusCode, header};
use propolis_client::api;
use slog::{error, info, o};
use thiserror::Error;
use uuid::Uuid;

use crate::server::Context;

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

/// Errors which may occur during the course of a migration
#[derive(Error, Debug)]
pub enum MigrateError {
    #[error("{0}")]
    Http(#[from] hyper::Error),

    #[error("couldn't establish migration connection to source instance")]
    Initiate,

    #[error("the source ({0}) and destination ({1}) instances are incompatible for migration")]
    Incompatible(String, String),

    #[error("expected connection upgrade")]
    UpgradeExpected,
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
            | MigrateError::Incompatible(_, _) => {
                HttpError::for_internal_error(msg)
            }
            MigrateError::UpgradeExpected => {
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
) -> Result<Response<Body>, MigrateError> {
    // Create a new log context for the migration
    let log = rqctx.log.new(o!("migrate_role" => "source"));

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

    // We've successfully negotiated a migration protocol w/ the destination.
    // Now, we spawn a new task to handle the actual migration over the upgraded socket
    let task = tokio::spawn(async move {});

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
    migrate_info: api::InstanceMigrateStartRequest,
) -> Result<HttpResponseOk<()>, MigrateError> {
    // Create a new log context for the migration
    let log = rqctx.log.new(o!(
        "migrate_role" => "destination",
        "migrate_src_addr" => migrate_info.src_addr.clone()
    ));

    // TODO: https
    // TODO: We need to make sure the src_addr is a valid target
    // TODO: will src_uuid be different than dst_uuid (i.e. instance_id)?
    let src_migrate_url = format!(
        "http://{}/instances/{}/migrate/start",
        migrate_info.src_addr, migrate_info.src_uuid
    );
    info!(log, "Begin migration"; "src_migrate_url" => &src_migrate_url);

    // Build upgrade request to the source instance
    let dst_protocol = MIGRATION_PROTOCOL_STR;
    let req = hyper::Request::builder()
        .method(Method::PUT)
        .uri(src_migrate_url)
        .header(header::CONNECTION, "upgrade")
        // TODO: move to constant
        .header(header::UPGRADE, dst_protocol)
        .body(Body::empty())
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

    // Good to go, ready to migrate from the source via `conn`
    // TODO: wrap in a tokio codec::Framed or such

    Ok(HttpResponseOk(()))
}
