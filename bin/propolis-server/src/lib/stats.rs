// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Methods for starting an Oximeter endpoint and gathering server-level stats.

use dropshot::{
    ConfigDropshot, ConfigLogging, ConfigLoggingLevel, HandlerTaskMode,
};
use omicron_common::api::internal::nexus::ProducerEndpoint;
use oximeter::{
    types::{Cumulative, Sample},
    Metric, MetricsError, Producer, Target,
};
use oximeter_producer::{Config, Server};
use slog::{error, info, warn, Logger};

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::server::MetricsEndpointConfig;

const OXIMETER_STAT_INTERVAL: tokio::time::Duration =
    tokio::time::Duration::from_secs(30);

/// The Oximeter `Target` for server-level stats. Contains the identifiers that
/// are used to identify a specific incarnation of an instance as a source of
/// metric data.
#[derive(Debug, Copy, Clone, Target)]
struct InstanceUuid {
    pub uuid: Uuid,
}

/// An Oximeter `Metric` that specifies the number of times an instance was
/// reset via the server API.
#[derive(Debug, Default, Copy, Clone, Metric)]
struct Reset {
    /// The number of times this instance was reset via the API.
    #[datum]
    pub count: Cumulative<i64>,
}

/// The full set of server-level metrics, collated by
/// [`ServerStatsOuter::produce`] into the types needed to relay these
/// statistics to Oximeter.
#[derive(Clone, Debug)]
struct ServerStats {
    /// The name to use as the Oximeter target, i.e. the identifier of the
    /// source of these metrics.
    stat_name: InstanceUuid,

    /// The reset count for the relevant instance.
    run_count: Reset,
}

impl ServerStats {
    pub fn new(uuid: Uuid) -> Self {
        ServerStats {
            stat_name: InstanceUuid { uuid },
            run_count: Default::default(),
        }
    }
}

/// The public wrapper for server-level metrics.
#[derive(Clone, Debug)]
pub struct ServerStatsOuter {
    server_stats_wrapped: Arc<Mutex<ServerStats>>,
}

impl ServerStatsOuter {
    /// Increments the number of times the instance was reset.
    pub fn count_reset(&self) {
        let mut inner = self.server_stats_wrapped.lock().unwrap();
        let datum = inner.run_count.datum_mut();
        *datum += 1;
    }
}

impl Producer for ServerStatsOuter {
    fn produce(
        &mut self,
    ) -> Result<Box<dyn Iterator<Item = Sample> + 'static>, MetricsError> {
        let mut data = Vec::with_capacity(1);
        let inner = self.server_stats_wrapped.lock().unwrap();
        let name = inner.stat_name;
        data.push(Sample::new(&name, &inner.run_count));
        Ok(Box::new(data.into_iter()))
    }
}

/// Launches and returns an Oximeter metrics server.
///
/// # Parameters
///
/// - `id`: The ID of the instance for whom this server is being started.
/// - `my_address`: The address of this Propolis process. Oximeter will connect
///   to this to query metrics.
/// - `registration_address`: The address of the Oximeter server that will be
///   told how to connect to the metric server this routine starts.
/// - `log`: A logger to use when logging from this routine.
pub async fn start_oximeter_server(
    id: Uuid,
    config: &MetricsEndpointConfig,
    log: Logger,
) -> Option<Server> {
    // Request an ephemeral port on which to serve metrics.
    let my_address = SocketAddr::new(config.propolis_addr.ip(), 0);
    let registration_address = config.metric_addr;
    info!(
        log,
        "Attempt to register {:?} with Nexus/Oximeter at {:?}",
        my_address,
        registration_address
    );

    let dropshot_config = ConfigDropshot {
        bind_address: my_address,
        request_body_max_bytes: 2048,
        default_handler_task_mode: HandlerTaskMode::Detached,
    };

    let logging_config =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Info };

    let server_info = ProducerEndpoint {
        id,
        address: my_address,
        base_route: "/collect".to_string(),
        interval: OXIMETER_STAT_INTERVAL,
    };

    let config = Config {
        server_info,
        registration_address,
        dropshot_config,
        logging_config,
    };

    const N_RETRY: u8 = 2;
    const RETRY_WAIT_SEC: u64 = 1;
    for _ in 0..N_RETRY {
        let server = Server::start(&config).await;
        match server {
            Ok(server) => {
                info!(
                    log,
                    "connected {:?} to oximeter {:?}",
                    my_address,
                    registration_address
                );
                return Some(server);
            }
            Err(e) => {
                warn!(
                    log,
                    "Could not connect to oximeter (retrying in {}s):\n{}",
                    RETRY_WAIT_SEC,
                    e,
                );

                tokio::time::sleep(tokio::time::Duration::from_secs(
                    RETRY_WAIT_SEC,
                ))
                .await;
            }
        }
    }
    error!(log, "Could not connect to oximeter after {} retries", N_RETRY);

    None
}

/// Creates and registers a set of server-level metrics for an instance.
pub fn register_server_metrics(
    id: Uuid,
    server: &Server,
) -> anyhow::Result<ServerStatsOuter> {
    let stats = ServerStats::new(id);
    let stats_outer =
        ServerStatsOuter { server_stats_wrapped: Arc::new(Mutex::new(stats)) };

    server.registry().register_producer(stats_outer.clone())?;

    Ok(stats_outer)
}
