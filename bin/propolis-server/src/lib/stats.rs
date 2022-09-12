//! Methods for starting an Oximeter endpoint and gathering server-level stats.

// Copyright 2022 Oxide Computer Company

use anyhow::anyhow;
use dropshot::{ConfigDropshot, ConfigLogging, ConfigLoggingLevel};
use omicron_common::api::internal::nexus::ProducerEndpoint;
use oximeter::{
    types::{Cumulative, Sample},
    Metric, MetricsError, Producer, Target,
};
use oximeter_producer::{Config, Server};
use slog::{error, info, Logger};

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
/// - `plog`: A logger to use when logging from this routine.
pub async fn start_oximeter_server(
    id: Uuid,
    config: &MetricsEndpointConfig,
    plog: Logger,
) -> anyhow::Result<Server> {
    let my_address = config.propolis_addr;
    let registration_address = config.metric_addr;
    info!(
        plog,
        "Attempt to register {:?} with Nexus/Oximeter at {:?}",
        my_address,
        registration_address
    );

    let dropshot_config = ConfigDropshot {
        bind_address: my_address,
        request_body_max_bytes: 2048,
        tls: None,
    };

    let logging_config =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Error };
    let log = logging_config
        .to_logger("propolis-metric-server")
        .map_err(|error| anyhow!("failed to create logger: {}", error))?;

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

    let mut retry_print_timeout = 0;
    loop {
        let server = Server::start(&config).await;
        match server {
            Ok(server) => {
                info!(
                    log,
                    "connected {:?} to oximeter {:?}",
                    my_address,
                    registration_address
                );
                return Ok(server);
            }
            Err(e) => {
                if retry_print_timeout == 0 {
                    error!(log, "Can't connect to oximeter server:\n{}", e);
                    retry_print_timeout = 1;
                }
                // Retry every 10 seconds, but only print once a minute
                retry_print_timeout = (retry_print_timeout + 1) % 7;
                tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            }
        }
    }
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
