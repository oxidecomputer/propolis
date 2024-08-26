// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Methods for starting an Oximeter endpoint and gathering server-level stats.

use omicron_common::api::internal::nexus::ProducerEndpoint;
use omicron_common::api::internal::nexus::ProducerKind;
use oximeter::{
    types::{ProducerRegistry, Sample},
    MetricsError, Producer,
};
use oximeter_producer::{Config, Error, Server};
use propolis_api_types::InstanceProperties;
use slog::Logger;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::server::MetricsEndpointConfig;
use crate::stats::virtual_machine::{Reset, VirtualMachine};
use oximeter_instruments::kstat::KstatSampler;

mod pvpanic;
pub(crate) mod virtual_disk;
pub(crate) mod virtual_machine;
pub use self::pvpanic::PvpanicProducer;

// Interval on which we ask `oximeter` to poll us for metric data.
//
// Note that some statistics, like those based on kstats, are sampled more
// densely than this proactively. Their sampling rate is decoupled from this
// poll interval. Others, like the virtual disk stats, are updated all the time,
// but we only generate _samples_ from that when `oximeter` comes polling.
//
// In short, set this to the minimum interval on which you'd like those
// statistics to be sampled.
const OXIMETER_COLLECTION_INTERVAL: tokio::time::Duration =
    tokio::time::Duration::from_secs(10);

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
const KSTAT_LIMIT_PER_VCPU: u32 =
    crate::stats::virtual_machine::N_VCPU_MICROSTATES * 64;

/// Shared type for tracking metrics about the Propolis API server itself.
#[derive(Clone, Debug)]
struct ServerStatsInner {
    /// The oximeter Target identifying this instance as the source of metric
    /// data.
    virtual_machine: VirtualMachine,

    /// The reset count for the relevant instance.
    run_count: Reset,
}

impl ServerStatsInner {
    pub fn new(virtual_machine: VirtualMachine) -> Self {
        ServerStatsInner {
            virtual_machine,
            run_count: Reset { datum: Default::default() },
        }
    }
}

/// Type publishing metrics about the Propolis API server itself.
//
// NOTE: This type is shared with the server and the oximeter producer. The
// former updates stats as API requests are handled or other actions taken, and
// the latter collects the stats when oximeter requests them.
#[derive(Clone, Debug)]
pub struct ServerStats {
    inner: Arc<Mutex<ServerStatsInner>>,
}

impl ServerStats {
    /// Create new server stats, representing the provided instance.
    pub fn new(vm: VirtualMachine) -> Self {
        Self { inner: Arc::new(Mutex::new(ServerStatsInner::new(vm))) }
    }

    /// Increments the number of times the managed instance was reset.
    pub fn count_reset(&self) {
        self.inner.lock().unwrap().run_count.datum.increment();
    }
}

impl Producer for ServerStats {
    fn produce(
        &mut self,
    ) -> Result<Box<dyn Iterator<Item = Sample> + 'static>, MetricsError> {
        let run_count = {
            let inner = self.inner.lock().unwrap();
            std::iter::once(Sample::new(
                &inner.virtual_machine,
                &inner.run_count,
            )?)
        };
        Ok(Box::new(run_count))
    }
}

/// Launches and returns an Oximeter metrics server.
///
/// # Parameters
///
/// - `id`: The ID of the instance for whom this server is being started.
/// - `config`: The metrics config options, including our address (on which we
///    serve metrics for oximeter to collect), and the registration address (a
///    Nexus instance through which we request registration as an oximeter
///    producer).
/// - `log`: A logger to use when logging from this routine.
/// - `registry`: The oximeter [`ProducerRegistry`] that the spawned server will
///    use to return metric data to oximeter on request.
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
    let producer_address = SocketAddr::new(config.listen_addr, 0);
    let registration_address = config.registration_addr;

    let server_info = ProducerEndpoint {
        id,
        kind: ProducerKind::Instance,
        address: producer_address,
        interval: OXIMETER_COLLECTION_INTERVAL,
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
        registration_address,
        request_body_max_bytes: MAX_REQUEST_SIZE,
        log: producer_log,
    };

    // Create the server which will attempt to register with Nexus.
    Server::with_registry(registry.clone(), &config)
}

/// Create an object that can be used to sample kstat-based metrics.
pub(crate) fn create_kstat_sampler(
    log: &Logger,
    n_vcpus: u32,
) -> Option<KstatSampler> {
    let kstat_limit = usize::try_from(n_vcpus * KSTAT_LIMIT_PER_VCPU).unwrap();
    match KstatSampler::with_sample_limit(log, kstat_limit) {
        Ok(sampler) => Some(sampler),
        Err(e) => {
            slog::error!(
                log,
                "failed to create KstatSampler, \
                kstat-based stats will be unavailable";
                "error" => ?e,
            );
            None
        }
    }
}

/// Track kstats required to publish vCPU metrics for this instance.
#[cfg(any(test, not(target_os = "illumos")))]
pub(crate) async fn track_vcpu_kstats(
    log: &Logger,
    _: &KstatSampler,
    _: &InstanceProperties,
) {
    slog::error!(log, "vCPU stats are not supported on this platform");
}

/// Track kstats required to publish vCPU metrics for this instance.
#[cfg(all(not(test), target_os = "illumos"))]
pub(crate) async fn track_vcpu_kstats(
    log: &Logger,
    sampler: &KstatSampler,
    properties: &InstanceProperties,
) {
    let virtual_machine = VirtualMachine::from(properties);
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
}
