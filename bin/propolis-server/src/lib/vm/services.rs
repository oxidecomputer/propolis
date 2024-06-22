// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Services visible to consumers outside this Propolis that depend on
//! functionality supplied by an extant VM.

use std::sync::Arc;

use rfb::server::VncServer;
use slog::{error, Logger};

use crate::{serial::SerialTaskControlMessage, vnc::PropolisVncServer};

#[derive(Default)]
struct OximeterState {
    server: Option<oximeter_producer::Server>,
    stats: Option<crate::stats::ServerStatsOuter>,
}

pub(super) struct VmServices {
    serial_task: tokio::sync::Mutex<Option<crate::serial::SerialTask>>,
    oximeter: tokio::sync::Mutex<OximeterState>,
    vnc_server: Arc<VncServer<PropolisVncServer>>,
}

impl VmServices {
    pub(super) async fn stop(&self, log: &Logger) {
        self.vnc_server.stop().await;

        if let Some(serial_task) = self.serial_task.lock().await.take() {
            let _ = serial_task
                .control_ch
                .send(SerialTaskControlMessage::Stopping)
                .await;
            let _ = serial_task.task.await;
        }

        let mut oximeter_state = self.oximeter.lock().await;
        if let Some(server) = oximeter_state.server.take() {
            if let Err(e) = server.close().await {
                error!(log, "failed to close oximeter producer server";
                       "error" => ?e);
            }
        }

        let _ = oximeter_state.stats.take();
    }
}
