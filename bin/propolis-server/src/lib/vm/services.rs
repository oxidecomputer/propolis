// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Services visible to consumers outside this Propolis that depend on
//! functionality supplied by an extant VM.

use std::sync::Arc;

use oximeter::types::ProducerRegistry;
use rfb::server::VncServer;
use slog::{error, info, Logger};

use crate::{
    serial::SerialTaskControlMessage, server::MetricsEndpointConfig,
    stats::virtual_machine::VirtualMachine, vnc::PropolisVncServer,
};

use super::VmObjects;

#[derive(Default)]
pub(crate) struct OximeterState {
    server: Option<oximeter_producer::Server>,
    pub stats: Option<crate::stats::ServerStatsOuter>,
}

pub(crate) struct VmServices {
    pub serial_task: tokio::sync::Mutex<Option<crate::serial::SerialTask>>,
    pub oximeter: tokio::sync::Mutex<OximeterState>,
    pub vnc_server: Arc<VncServer<PropolisVncServer>>,
}

impl VmServices {
    pub(super) async fn new(
        log: &slog::Logger,
        vm: &Arc<super::Vm>,
        vm_objects: &VmObjects,
        vm_properties: &propolis_api_types::InstanceProperties,
        ensure_options: &super::EnsureOptions,
    ) -> Self {
        let oximeter_state = if let Some(cfg) = &ensure_options.metrics_config {
            let registry = ensure_options.oximeter_registry.as_ref().expect(
                "should have a producer registry if metrics are configured",
            );

            let state =
                register_oximeter_producer(log, cfg, registry, vm_properties)
                    .await;

            state
        } else {
            OximeterState::default()
        };

        let vnc_server = ensure_options.vnc_server.clone();
        if let Some(ramfb) = &vm_objects.framebuffer {
            vnc_server.server.initialize(
                crate::vnc::RamFb::new(ramfb.get_framebuffer_spec()),
                vm_objects.ps2ctrl.clone(),
                vm.clone(),
            );

            let notifier_server_ref = vnc_server.clone();
            let rt = tokio::runtime::Handle::current();
            ramfb.set_notifier(Box::new(move |config, is_valid| {
                let vnc = notifier_server_ref.clone();
                rt.block_on(vnc.server.update(config, is_valid, &vnc));
            }));
        }

        let serial_task = start_serial_task(log, vm_objects).await;

        Self {
            serial_task: tokio::sync::Mutex::new(Some(serial_task)),
            oximeter: tokio::sync::Mutex::new(oximeter_state),
            vnc_server,
        }
    }

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

async fn register_oximeter_producer(
    log: &slog::Logger,
    cfg: &MetricsEndpointConfig,
    registry: &ProducerRegistry,
    vm_properties: &propolis_api_types::InstanceProperties,
) -> OximeterState {
    let mut oximeter_state = OximeterState::default();
    let virtual_machine = VirtualMachine::from(vm_properties);

    // Create the server itself.
    //
    // The server manages all details of the registration with Nexus, so we
    // don't need our own task for that or way to shut it down.
    oximeter_state.server = match crate::stats::start_oximeter_server(
        virtual_machine.instance_id,
        &cfg,
        &log,
        registry,
    ) {
        Ok(server) => {
            info!(log, "created metric producer server");
            Some(server)
        }
        Err(err) => {
            error!(
                log,
                "failed to construct metric producer server, \
                no metrics will be available for this instance.";
                "error" => ?err,
            );
            None
        }
    };

    // Assign our own metrics production for this VM instance to the
    // registry, letting the server actually return them to oximeter when
    // polled.
    oximeter_state.stats = match crate::stats::register_server_metrics(
        registry,
        virtual_machine,
        &log,
    )
    .await
    {
        Ok(stats) => Some(stats),
        Err(e) => {
            error!(
                log,
                "failed to register our server metrics with \
                the ProducerRegistry, no server stats will \
                be produced";
                "error" => ?e,
            );

            None
        }
    };

    oximeter_state
}

async fn start_serial_task(
    log: &slog::Logger,
    vm_objects: &VmObjects,
) -> crate::serial::SerialTask {
    let (websocks_ch, websocks_recv) = tokio::sync::mpsc::channel(1);
    let (control_ch, control_recv) = tokio::sync::mpsc::channel(1);

    let serial = vm_objects.com1().clone();
    serial.set_task_control_sender(control_ch.clone()).await;
    let err_log = log.new(slog::o!("component" => "serial task"));
    let task = tokio::spawn(async move {
        if let Err(e) = crate::serial::instance_serial_task(
            websocks_recv,
            control_recv,
            serial,
            err_log.clone(),
        )
        .await
        {
            error!(err_log, "Failure in serial task: {}", e);
        }
    });

    crate::serial::SerialTask { task, control_ch, websocks_ch }
}
