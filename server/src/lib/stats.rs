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

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

// How frequently Oximeter will collect metrics.
const OXIMETER_STAT_INTERVAL: u64 = 30;

// These structs are used to construct the desired metrics for Oximeter.
#[derive(Debug, Copy, Clone, Target)]
struct InstanceUuid {
    pub uuid: Uuid,
}
#[derive(Debug, Default, Copy, Clone, Metric)]
pub struct Reset {
    /// Count of times instance was rebooted
    #[datum]
    pub count: Cumulative<i64>,
}

// All the counter metrics in one struct.
// To create additional metrics that Oximeter will collect, add fields to this
// structure.  See Oximeter for details, but each fields should be
// constructed similar to "run_count". A new method should be added to the
// PropStatOuter impl that will be called when the new field has changed.
// The produce method should be updated as well.
#[derive(Clone, Debug)]
pub struct PropCountStat {
    stat_name: InstanceUuid,
    run_count: Reset,
}

impl PropCountStat {
    pub fn new(uuid: Uuid) -> Self {
        PropCountStat {
            stat_name: InstanceUuid { uuid: uuid },
            run_count: Default::default(),
        }
    }
    pub fn uuid(&self) -> Uuid {
        return self.stat_name.uuid;
    }
}

// This struct wraps the stat struct in an Arc/Mutex so the worker tasks can
// share it with the producer trait.
#[derive(Clone, Debug)]
pub struct PropStatOuter {
    pub prop_stat_wrap: Arc<Mutex<PropCountStat>>,
}

impl PropStatOuter {
    // When an operation happens that we wish to record in Oximeter,
    // one of these methods will be called.  Each method will get the
    // correct field of PropCountStat to record the update.
    pub fn count_reset(&self) {
        let mut pso = self.prop_stat_wrap.lock().unwrap();
        let datum = pso.run_count.datum_mut();
        *datum += 1;
    }
}

// This trait is what is called to update the data that is collected
// by Oximeter every OXIMETER_STAT_INTERVAL seconds.
impl Producer for PropStatOuter {
    fn produce(
        &mut self,
    ) -> Result<Box<dyn Iterator<Item = Sample> + 'static>, MetricsError> {
        let pso = self.prop_stat_wrap.lock().unwrap();

        let mut data = Vec::with_capacity(1);
        let name = pso.stat_name;

        data.push(Sample::new(&name, &pso.run_count));

        // Yield the available samples.
        Ok(Box::new(data.into_iter()))
    }
}

/// Setup Oximeter
/// This starts a dropshot server, and then registers the PropStatOuter
/// producer with Oximeter.
/// Once registered, we return the server to the caller.
/// By returning the server to the caller, we allow the ProducerRegister
/// to be cloned and passed to any library that wishes to record metrics.
pub async fn prop_oximeter(
    id: Uuid,
    my_address: SocketAddr,
    registration_address: SocketAddr,
    plog: Logger,
) -> anyhow::Result<Server> {
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
        interval: tokio::time::Duration::from_secs(OXIMETER_STAT_INTERVAL),
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
