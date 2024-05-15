// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Methods for starting an Oximeter endpoint and gathering server-level stats.

use omicron_common::api::internal::nexus::ProducerEndpoint;
use omicron_common::api::internal::nexus::ProducerKind;
use oximeter::{
    types::{Cumulative, ProducerRegistry, Sample},
    Metric, MetricsError, Producer,
};
use oximeter_producer::{Config, Error, Server};
use slog::{info, Logger};

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::server::MetricsEndpointConfig;
use crate::stats::virtual_machine::VirtualMachine;

// Propolis is built and some tests are run on non-illumos systems. The real
// `kstat` infrastructure cannot be built there, so some conditional compilation
// tricks are needed
#[cfg(all(not(test), target_os = "illumos"))]
use oximeter_instruments::kstat::KstatSampler;

mod pvpanic;
pub(crate) mod virtual_machine;
pub use self::pvpanic::PvpanicProducer;

// Interval on which we ask `oximeter` to poll us for metric data.
const OXIMETER_STAT_INTERVAL: tokio::time::Duration =
    tokio::time::Duration::from_secs(30);

// Interval on which we produce vCPU metrics.
#[cfg(all(not(test), target_os = "illumos"))]
const VCPU_KSTAT_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5);

// The kstat sampler includes a limit to its internal buffers for each target,
// to avoid growing without bound. This defaults to 500 samples. Since we have 5
// vCPU microstates for which we track occupancy and up to 64 vCPUs, we can
// easily run up against this default.
//
// This limit provides extra space for up to 64 samples per vCPU per microstate,
// to ensure we don't throw away too much data if oximeter cannot reach us.
#[cfg(all(not(test), target_os = "illumos"))]
const KSTAT_LIMIT_PER_VCPU: u32 =
    crate::stats::virtual_machine::N_VCPU_MICROSTATES * 64;

/// An Oximeter `Metric` that specifies the number of times an instance was
/// reset via the server API.
#[derive(Debug, Default, Copy, Clone, Metric)]
struct Reset {
    /// The number of times this instance was reset via the API.
    #[datum]
    pub count: Cumulative<u64>,
}

/// The full set of server-level metrics, collated by
/// [`ServerStatsOuter::produce`] into the types needed to relay these
/// statistics to Oximeter.
#[derive(Clone, Debug)]
struct ServerStats {
    /// The oximeter Target identifying this instance as the source of metric
    /// data.
    virtual_machine: VirtualMachine,

    /// The reset count for the relevant instance.
    run_count: Reset,
}

impl ServerStats {
    pub fn new(virtual_machine: VirtualMachine) -> Self {
        ServerStats { virtual_machine, run_count: Default::default() }
    }
}

/// The public wrapper for server-level metrics.
#[derive(Clone, Debug)]
pub struct ServerStatsOuter {
    server_stats_wrapped: Arc<Mutex<ServerStats>>,
    #[cfg(all(not(test), target_os = "illumos"))]
    kstat_sampler: Option<KstatSampler>,
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
        let run_count = {
            let inner = self.server_stats_wrapped.lock().unwrap();
            std::iter::once(Sample::new(
                &inner.virtual_machine,
                &inner.run_count,
            )?)
        };
        #[cfg(all(not(test), target_os = "illumos"))]
        if let Some(sampler) = self.kstat_sampler.as_mut() {
            let samples = sampler.produce()?;
            return Ok(Box::new(run_count.chain(samples)));
        }
        Ok(Box::new(run_count))
    }
}

/// Launches and returns an Oximeter metrics server.
///
/// # Parameters
///
/// - `id`: The ID of the instance for whom this server is being started.
/// - `config`: The metrics config options, including our address (on which we
/// serve metrics for oximeter to collect), and the registration address (a
/// Nexus instance through which we request registration as an oximeter
/// producer).
/// - `log`: A logger to use when logging from this routine.
/// - `registry`: The oximeter [`ProducerRegistry`] that the spawned server will
/// use to return metric data to oximeter on request.
///
/// This method attempts to register a _single time_ with Nexus. Callers should
/// arrange for this to be called continuously if desired, such as with a
/// backoff policy.
///
/// The returned server will attempt to register with Nexus in a background
/// task, and will periodically renew that registration. The returned server is
/// running, and need not be poked or renewed to successfully serve metric data.
pub fn start_oximeter_server(
    id: Uuid,
    config: &MetricsEndpointConfig,
    log: &Logger,
    registry: &ProducerRegistry,
) -> Result<Server, Error> {
    // Request an ephemeral port on which to serve metrics.
    let producer_address = SocketAddr::new(config.propolis_addr.ip(), 0);
    let registration_address = config.metric_addr;
    info!(
        log,
        "Attempting to register with Nexus as a metric producer";
        "producer_address" => %producer_address,
        "nexus_address" => %registration_address,
    );

    let server_info = ProducerEndpoint {
        id,
        kind: ProducerKind::Instance,
        address: producer_address,
        interval: OXIMETER_STAT_INTERVAL,
    };

    // Create a child logger, to avoid intermingling the producer server output
    // with the main Propolis server.
    let producer_log = oximeter_producer::LogConfig::Logger(
        log.new(slog::o!("component" => "oximeter-producer")),
    );

    // The maximum size of a single Dropshot request.
    //
    // This is a pretty arbitrary limit, but one that should be big enough for
    // the statistics we serve today (vCPU usage and panic counts), with
    // headroom for adding quite a few more.
    const MAX_REQUEST_SIZE: usize = 1024 * 1024;
    let config = Config {
        server_info,
        registration_address: Some(registration_address),
        request_body_max_bytes: MAX_REQUEST_SIZE,
        log: producer_log,
    };

    // Create the server which will attempt to register with Nexus.
    Server::with_registry(registry.clone(), &config)
}

/// Creates and registers a set of server-level metrics for an instance.
///
/// This attempts to initialize kstat-based metrics for vCPU usage data. This
/// may fail, in which case those metrics will be unavailable.
//
// NOTE: The logger is unused if we don't pass it to `setup_kstat_tracking`
// internally, so ignore that clippy lint.
#[cfg_attr(not(all(not(test), target_os = "illumos")), allow(unused_variables))]
pub async fn register_server_metrics(
    registry: &ProducerRegistry,
    virtual_machine: VirtualMachine,
    log: &Logger,
) -> anyhow::Result<ServerStatsOuter> {
    let stats = ServerStats::new(virtual_machine.clone());

    let stats_outer = ServerStatsOuter {
        server_stats_wrapped: Arc::new(Mutex::new(stats)),
        // Setup the collection of kstats for this instance.
        #[cfg(all(not(test), target_os = "illumos"))]
        kstat_sampler: setup_kstat_tracking(log, virtual_machine).await,
    };

    registry.register_producer(stats_outer.clone())?;

    Ok(stats_outer)
}

#[cfg(all(not(test), target_os = "illumos"))]
async fn setup_kstat_tracking(
    log: &Logger,
    virtual_machine: VirtualMachine,
) -> Option<KstatSampler> {
    let kstat_limit =
        usize::try_from(virtual_machine.n_vcpus() * KSTAT_LIMIT_PER_VCPU)
            .unwrap();
    match KstatSampler::with_sample_limit(log, kstat_limit) {
        Ok(sampler) => {
            let details = oximeter_instruments::kstat::CollectionDetails::never(
                VCPU_KSTAT_INTERVAL,
            );
            if let Err(e) = sampler.add_target(virtual_machine, details).await {
                slog::error!(
                    log,
                    "failed to add VirtualMachine target, \
                    vCPU stats will be unavailable";
                    "error" => ?e,
                );
            }
            Some(sampler)
        }
        Err(e) => {
            slog::error!(
                log,
                "failed to create KstatSampler, \
                vCPU stats will be unavailable";
                "error" => ?e,
            );
            None
        }
    }
}
