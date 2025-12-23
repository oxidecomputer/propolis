// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use dropshot::{
    HttpError, HttpResponseCreated, HttpResponseOk,
    HttpResponseUpdatedNoContent, Path, Query, RequestContext, TypedBody,
    WebsocketChannelResult, WebsocketConnection,
};
use dropshot_api_manager_types::api_versions;
use propolis_api_types_versions::{latest, v1};

api_versions!([
    // WHEN CHANGING THE API (part 1 of 2):
    //
    // +- Pick a new semver and define it in the list below.  The list MUST
    // |  remain sorted, which generally means that your version should go at
    // |  the very top.
    // |
    // |  Duplicate this line, uncomment the *second* copy, update that copy for
    // |  your new API version, and leave the first copy commented out as an
    // |  example for the next person.
    // v
    // (next_int, IDENT),
    (2, PROGRAMMABLE_SMBIOS),
    (1, INITIAL),
]);

// WHEN CHANGING THE API (part 2 of 2):
//
// The call to `api_versions!` above defines constants of type
// `semver::Version` that you can use in your Dropshot API definition to specify
// the version when a particular endpoint was added or removed.  For example, if
// you used:
//
//     (2, ADD_FOOBAR)
//
// Then you could use `VERSION_ADD_FOOBAR` as the version in which endpoints
// were added or removed.

#[dropshot::api_description]
pub trait PropolisServerApi {
    type Context;

    #[endpoint {
        method = PUT,
        path = "/instance",
        versions = VERSION_PROGRAMMABLE_SMBIOS..
    }]
    async fn instance_ensure(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<latest::instance::InstanceEnsureRequest>,
    ) -> Result<
        HttpResponseCreated<latest::instance::InstanceEnsureResponse>,
        HttpError,
    >;

    #[endpoint {
        operation_id = "instance_ensure",
        method = PUT,
        path = "/instance",
        versions = ..VERSION_PROGRAMMABLE_SMBIOS
    }]
    async fn instance_ensure_v1(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<v1::instance::InstanceEnsureRequest>,
    ) -> Result<
        HttpResponseCreated<latest::instance::InstanceEnsureResponse>,
        HttpError,
    > {
        Self::instance_ensure(
            rqctx,
            request.map(latest::instance::InstanceEnsureRequest::from),
        )
        .await
    }

    #[endpoint {
        method = GET,
        path = "/instance/spec",
        versions = VERSION_PROGRAMMABLE_SMBIOS..
    }]
    async fn instance_spec_get(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<
        HttpResponseOk<latest::instance_spec::InstanceSpecGetResponse>,
        HttpError,
    >;

    #[endpoint {
        operation_id = "instance_spec_get",
        method = GET,
        path = "/instance/spec",
        versions = ..VERSION_PROGRAMMABLE_SMBIOS
    }]
    async fn instance_spec_get_v1(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<
        HttpResponseOk<v1::instance_spec::InstanceSpecGetResponse>,
        HttpError,
    > {
        Ok(Self::instance_spec_get(rqctx)
            .await?
            .map(v1::instance_spec::InstanceSpecGetResponse::from))
    }

    #[endpoint {
        method = GET,
        path = "/instance",
    }]
    async fn instance_get(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<latest::instance::InstanceGetResponse>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/instance/state-monitor",
    }]
    async fn instance_state_monitor(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<latest::instance::InstanceStateMonitorRequest>,
    ) -> Result<
        HttpResponseOk<latest::instance::InstanceStateMonitorResponse>,
        HttpError,
    >;

    #[endpoint {
        method = PUT,
        path = "/instance/state",
    }]
    async fn instance_state_put(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<latest::instance::InstanceStateRequested>,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError>;

    #[endpoint {
        method = GET,
        path = "/instance/serial/history",
    }]
    async fn instance_serial_history_get(
        rqctx: RequestContext<Self::Context>,
        query: Query<latest::serial::InstanceSerialConsoleHistoryRequest>,
    ) -> Result<
        HttpResponseOk<latest::serial::InstanceSerialConsoleHistoryResponse>,
        HttpError,
    >;

    #[channel {
        protocol = WEBSOCKETS,
        path = "/instance/serial",
    }]
    async fn instance_serial(
        rqctx: RequestContext<Self::Context>,
        query: Query<latest::serial::InstanceSerialConsoleStreamRequest>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult;

    // See the note on instance_migrate_start below. /instance/vnc is not
    // currently used (as of 2025-10), but before it's used we'll want to think
    // about versioning considerations for the WebSocket protocol, similar to
    // instance_migrate_start.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/instance/vnc",
        unpublished = true,
    }]
    async fn instance_vnc(
        rqctx: RequestContext<Self::Context>,
        _query: Query<()>,
        websock: WebsocketConnection,
    ) -> dropshot::WebsocketChannelResult;

    /// DO NOT USE THIS IF YOU'RE NOT PROPOLIS-SERVER.
    ///
    /// Internal API called during a migration from a destination instance to
    /// the source instance as part of the HTTP connection upgrade used to
    /// establish the migration link. This API is exported via OpenAPI purely
    /// to verify that its shape hasn't changed.
    //
    // # Versioning notes
    //
    // This API is expected to work even if the source and destination
    // propolis-server instances are on different versions. There are two parts
    // to versioning:
    //
    // 1. The parameters passed into the initial request.
    // 2. The protocol used for WebSocket communication.
    //
    // Part 1 is verified by the Dropshot API manager. For part 2,
    // propolis-server has internal support for protocol negotiation.
    //
    // Note that we currently bypass Progenitor and always pass in
    // VERSION_INITIAL. See `migration_start_connect` in
    // propolis-server/src/lib/migrate/destination.rs for where we do it. If we
    // introduce a change to this API, we'll have to carefully consider version
    // skew between the source and destination servers.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/instance/migrate/{migration_id}/start",
    }]
    async fn instance_migrate_start(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::migration::InstanceMigrateStartRequest>,
        websock: WebsocketConnection,
    ) -> dropshot::WebsocketChannelResult;

    #[endpoint {
        method = GET,
        path = "/instance/migration-status"
    }]
    async fn instance_migrate_status(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<
        HttpResponseOk<latest::migration::InstanceMigrateStatusResponse>,
        HttpError,
    >;

    /// Issues a snapshot request to a crucible backend.
    #[endpoint {
        method = POST,
        path = "/instance/disk/{id}/snapshot/{snapshot_id}",
    }]
    async fn instance_issue_crucible_snapshot_request(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::disk::SnapshotRequestPathParams>,
    ) -> Result<HttpResponseOk<()>, HttpError>;

    /// Gets the status of a Crucible volume backing a disk
    #[endpoint {
        method = GET,
        path = "/instance/disk/{id}/status",
    }]
    async fn disk_volume_status(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::disk::VolumeStatusPathParams>,
    ) -> Result<HttpResponseOk<latest::disk::VolumeStatus>, HttpError>;

    /// Issues a volume_construction_request replace to a crucible backend.
    #[endpoint {
        method = PUT,
        path = "/instance/disk/{id}/vcr",
    }]
    async fn instance_issue_crucible_vcr_request(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::disk::VCRRequestPathParams>,
        request: TypedBody<latest::disk::InstanceVCRReplace>,
    ) -> Result<HttpResponseOk<crucible_client_types::ReplaceResult>, HttpError>;

    /// Issues an NMI to the instance.
    #[endpoint {
        method = POST,
        path = "/instance/nmi",
    }]
    async fn instance_issue_nmi(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<()>, HttpError>;
}
