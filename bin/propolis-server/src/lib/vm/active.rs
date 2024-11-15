// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implements a wrapper around an active VM.

use std::sync::Arc;

use propolis_api_types::{
    instance_spec::SpecKey, InstanceProperties, InstanceStateRequested,
};
use slog::info;
use uuid::Uuid;

use crate::vm::request_queue::ExternalRequest;

use super::{
    objects::VmObjects, services::VmServices, CrucibleReplaceResultTx,
    InstanceStateRx, VmError,
};

/// The components and services that make up an active Propolis VM.
pub(crate) struct ActiveVm {
    /// The VM's logger.
    pub(super) log: slog::Logger,

    /// The input queue that receives external requests to change the VM's
    /// state.
    pub(super) state_driver_queue: Arc<super::state_driver::InputQueue>,

    /// Receives external state updates from the state driver.
    pub(super) external_state_rx: InstanceStateRx,

    /// The wrapped VM's properties.
    pub(super) properties: InstanceProperties,

    /// A reference to the wrapped VM's components. Callers with a reference to
    /// an `ActiveVm` can clone this to get a handle to those components.
    pub(super) objects: Arc<VmObjects>,

    /// Services that interact with VM users or the control plane outside the
    /// Propolis API (e.g. the serial console, VNC, and metrics reporting).
    pub(super) services: VmServices,

    /// The runtime on which this VM's state driver and any tasks spawned by
    /// the VM's components will run.
    pub(super) tokio_rt: tokio::runtime::Runtime,
}

impl ActiveVm {
    /// Yields a clonable reference to the active VM's components.
    pub(crate) fn objects(&self) -> &Arc<VmObjects> {
        &self.objects
    }

    /// Pushes a state change request to the VM's state change queue.
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

    /// Pushes a request to migrate out of a VM to the VM's state change queue.
    /// The migration protocol will communicate with the destination over the
    /// provided websocket.
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

    /// Pushes a request to reconfigure a Crucible volume to the VM's state
    /// change queue.
    ///
    /// # Arguments
    ///
    /// - `disk_name`: The name of the Crucible disk component (in the instance
    ///   spec) to modify.
    /// - `backend_id`: The UUID to use to find the Crucible backend in the
    ///   VM's Crucible backend map.
    /// - `new_vcr_json`: The new volume construction request to supply to the
    ///   selected backend.
    /// - `result_tx`: The channel to which the state driver should send the
    ///   replacement result after it completes this operation.
    pub(crate) fn reconfigure_crucible_volume(
        &self,
        backend_id: SpecKey,
        new_vcr_json: String,
        result_tx: CrucibleReplaceResultTx,
    ) -> Result<(), VmError> {
        self.state_driver_queue
            .queue_external_request(
                ExternalRequest::ReconfigureCrucibleVolume {
                    backend_id,
                    new_vcr_json,
                    result_tx,
                },
            )
            .map_err(Into::into)
    }

    /// Yields a reference to this VM's services.
    pub(crate) fn services(&self) -> &VmServices {
        &self.services
    }
}
