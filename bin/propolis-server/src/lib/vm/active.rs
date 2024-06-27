// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `ActiveVm` wrapper owns all of the components and services that make up
//! a running Propolis instance.

use std::sync::Arc;

use propolis_api_types::{InstanceProperties, InstanceStateRequested};
use slog::info;
use uuid::Uuid;

use crate::vm::request_queue::ExternalRequest;

use super::{
    objects::VmObjects, services::VmServices, CrucibleReplaceResultTx,
    InstanceStateRx, VmError,
};

/// The components and services that make up an active Propolis VM.
pub(crate) struct ActiveVm {
    pub(super) log: slog::Logger,
    pub(super) state_driver_queue: Arc<super::state_driver::InputQueue>,
    pub(super) external_state_rx: InstanceStateRx,
    pub(super) properties: InstanceProperties,
    pub(super) objects: Arc<VmObjects>,
    pub(super) services: VmServices,
}

impl ActiveVm {
    pub(crate) fn objects(&self) -> &Arc<VmObjects> {
        &self.objects
    }

    pub(crate) fn put_state(
        &self,
        requested: InstanceStateRequested,
    ) -> Result<(), VmError> {
        info!(self.log, "requested state via API";
              "state" => ?requested);

        self.state_driver_queue
            .queue_external_request(match requested {
                InstanceStateRequested::Run => ExternalRequest::Start,
                InstanceStateRequested::Stop => ExternalRequest::Stop,
                InstanceStateRequested::Reboot => ExternalRequest::Reboot,
            })
            .map_err(Into::into)
    }

    pub(crate) async fn request_migration_out(
        &self,
        migration_id: Uuid,
        websock: dropshot::WebsocketConnection,
    ) -> Result<(), VmError> {
        Ok(self.state_driver_queue.queue_external_request(
            ExternalRequest::MigrateAsSource {
                migration_id,
                websock: websock.into(),
            },
        )?)
    }

    pub(crate) fn reconfigure_crucible_volume(
        &self,
        disk_name: String,
        backend_id: Uuid,
        new_vcr_json: String,
        result_tx: CrucibleReplaceResultTx,
    ) -> Result<(), VmError> {
        self.state_driver_queue
            .queue_external_request(
                ExternalRequest::ReconfigureCrucibleVolume {
                    disk_name,
                    backend_id,
                    new_vcr_json,
                    result_tx,
                },
            )
            .map_err(Into::into)
    }

    pub(crate) fn services(&self) -> &VmServices {
        &self.services
    }
}
