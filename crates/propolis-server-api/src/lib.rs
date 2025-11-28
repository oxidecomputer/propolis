// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use dropshot::{
    HttpError, HttpResponseCreated, HttpResponseOk,
    HttpResponseUpdatedNoContent, Path, Query, RequestContext, TypedBody,
    WebsocketChannelResult, WebsocketConnection,
};
use propolis_api_types::{
    InstanceDiskAttachRequest, InstanceDiskDetachRequest,
    InstanceEnsureRequest, InstanceEnsureResponse, InstanceGetResponse,
    InstanceMigrateStartRequest, InstanceMigrateStatusResponse,
    InstanceSerialConsoleHistoryRequest, InstanceSerialConsoleHistoryResponse,
    InstanceSerialConsoleStreamRequest, InstanceSpecGetResponse,
    InstanceStateMonitorRequest, InstanceStateMonitorResponse,
    InstanceStateRequested, InstanceVCRReplace, SnapshotRequestPathParams,
    VCRRequestPathParams, VolumeStatus, VolumeStatusPathParams,
};

#[dropshot::api_description]
pub trait PropolisServerApi {
    type Context;

    #[endpoint {
        method = PUT,
        path = "/instance",
    }]
    async fn instance_ensure(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<InstanceEnsureRequest>,
    ) -> Result<HttpResponseCreated<InstanceEnsureResponse>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/instance/spec",
    }]
    async fn instance_spec_get(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<InstanceSpecGetResponse>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/instance",
    }]
    async fn instance_get(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<InstanceGetResponse>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/instance/state-monitor",
    }]
    async fn instance_state_monitor(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<InstanceStateMonitorRequest>,
    ) -> Result<HttpResponseOk<InstanceStateMonitorResponse>, HttpError>;

    #[endpoint {
        method = PUT,
        path = "/instance/state",
    }]
    async fn instance_state_put(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<InstanceStateRequested>,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError>;

    #[endpoint {
        method = GET,
        path = "/instance/serial/history",
    }]
    async fn instance_serial_history_get(
        rqctx: RequestContext<Self::Context>,
        query: Query<InstanceSerialConsoleHistoryRequest>,
    ) -> Result<HttpResponseOk<InstanceSerialConsoleHistoryResponse>, HttpError>;

    #[channel {
        protocol = WEBSOCKETS,
        path = "/instance/serial",
    }]
    async fn instance_serial(
        rqctx: RequestContext<Self::Context>,
        query: Query<InstanceSerialConsoleStreamRequest>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult;

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

    // This endpoint is meant to only be called during a migration from the
    // destination instance to the source instance as part of the HTTP connection
    // upgrade used to establish the migration link. We don't actually want this
    // exported via OpenAPI clients.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/instance/migrate/{migration_id}/start",
        unpublished = true,
    }]
    async fn instance_migrate_start(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<InstanceMigrateStartRequest>,
        websock: WebsocketConnection,
    ) -> dropshot::WebsocketChannelResult;

    #[endpoint {
        method = GET,
        path = "/instance/migration-status"
    }]
    async fn instance_migrate_status(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<InstanceMigrateStatusResponse>, HttpError>;

    /// Issues a snapshot request to a crucible backend.
    #[endpoint {
        method = POST,
        path = "/instance/disk/{id}/snapshot/{snapshot_id}",
    }]
    async fn instance_issue_crucible_snapshot_request(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<SnapshotRequestPathParams>,
    ) -> Result<HttpResponseOk<()>, HttpError>;

    /// Gets the status of a Crucible volume backing a disk
    #[endpoint {
        method = GET,
        path = "/instance/disk/{id}/status",
    }]
    async fn disk_volume_status(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<VolumeStatusPathParams>,
    ) -> Result<HttpResponseOk<VolumeStatus>, HttpError>;

    /// Issues a volume_construction_request replace to a crucible backend.
    #[endpoint {
        method = PUT,
        path = "/instance/disk/{id}/vcr",
    }]
    async fn instance_issue_crucible_vcr_request(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<VCRRequestPathParams>,
        request: TypedBody<InstanceVCRReplace>,
    ) -> Result<HttpResponseOk<crucible_client_types::ReplaceResult>, HttpError>;

    #[endpoint {
        method = PUT,
        path = "/instance/attach_disk",
    }]
    async fn instance_disk_attach(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<InstanceDiskAttachRequest>,
    ) -> Result<HttpResponseOk<()>, HttpError>;

    #[endpoint {
        method = PUT,
        path = "/instance/detach_disk",
    }]
    async fn instance_disk_detach(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<InstanceDiskDetachRequest>,
    ) -> Result<HttpResponseOk<()>, HttpError>;

    /// Issues an NMI to the instance.
    #[endpoint {
        method = POST,
        path = "/instance/nmi",
    }]
    async fn instance_issue_nmi(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<()>, HttpError>;
}
