use std::sync::Arc;

use dropshot::{HttpError, HttpResponseOk, RequestContext};
use hyper::{Body, Response};
use propolis_client::api;
use slog::{info, o};
use thiserror::Error;
use uuid::Uuid;

use crate::server::Context;

/// Errors which may occur during the course of a migration
#[derive(Error, Debug)]
pub enum MigrateError {}

impl Into<HttpError> for MigrateError {
    fn into(self) -> HttpError {
        HttpError::for_internal_error(format!("migration failed: {}", self))
    }
}

/// Begin the migration process (source-side).
///
///This will attempt to upgrade the given HTTP request to a `propolis-migrate`
/// connection and begin the migration in a separate task.
pub async fn source_start(
    rqctx: Arc<RequestContext<Context>>,
    instance_id: Uuid,
) -> Result<Response<Body>, HttpError> {
    todo!()
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
    let log =
        rqctx.log.new(o!("migrate_src_addr" => migrate_info.src_addr.clone()));

    // TODO: https
    // TODO: We need to make sure the src_addr is a valid target
    // TODO: will src_uuid be different than dst_uuid (i.e. instance_id)?
    let src_migrate_url = format!(
        "http://{}/instances/{}/migrate/start",
        migrate_info.src_addr, migrate_info.src_uuid
    );
    info!(log, "Begin migration"; "src_migrate_url" => &src_migrate_url);

    todo!()
}
