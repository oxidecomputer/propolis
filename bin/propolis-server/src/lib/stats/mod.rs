// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Methods for starting an Oximeter endpoint and gathering server-level stats.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use omicron_common::api::internal::nexus::{ProducerEndpoint, ProducerKind};
use oximeter::{
    types::{ProducerRegistry, Sample},
    MetricsError, Producer,
};
use oximeter_instruments::kstat::KstatSampler;
use oximeter_producer::{Config, Error, Server};
use propolis_api_types::{
    instance_spec::v0::InstanceSpecV0, InstanceProperties,
};
use slog::Logger;
use uuid::Uuid;

use crate::{server::MetricsEndpointConfig, vm::NetworkInterfaceIds};

mod network_interface;
mod pvpanic;
mod virtual_disk;
mod virtual_machine;

#[cfg(all(not(test), target_os = "illumos"))]
use self::network_interface::InstanceNetworkInterfaces;
pub(crate) use self::pvpanic::PvpanicProducer;
pub(crate) use self::virtual_disk::VirtualDiskProducer;
pub(crate) use self::virtual_machine::VirtualMachine;

/// Interval on which we ask `oximeter` to poll us for metric data.
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

/// Interval on which we sample instance/guest network interface metrics.
///
/// This matches what we're currently using for sampling
/// sled link metrics.
#[cfg(all(not(test), target_os = "illumos"))]
const NETWORK_INTERFACE_SAMPLE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(10);

/// Interval on which we produce vCPU metrics.
#[cfg(all(not(test), target_os = "illumos"))]
const VCPU_KSTAT_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5);

/// The kstat sampler includes a limit to its internal buffers for each target,
/// to avoid growing without bound. We introduce this buffer as a multiplier
/// for extra space.
const SAMPLE_BUFFER: u32 = 64;

/// The kstat sampler includes a limit to its internal buffers for each target,
/// to avoid growing without bound. This defaults to 500 samples. Since we have 5
/// vCPU microstates for which we track occupancy and up to 64 vCPUs, we can
/// easily run up against this default.
///
/// This limit provides extra space for up to 64 samples per vCPU per microstate,
/// to ensure we don't throw away too much data if oximeter cannot reach us.
const KSTAT_LIMIT_PER_VCPU: u32 =
    crate::stats::virtual_machine::N_VCPU_MICROSTATES * SAMPLE_BUFFER;

/// Shared type for tracking metrics about the Propolis API server itself.
#[derive(Clone, Debug)]
struct ServerStatsInner {
    /// The oximeter Target identifying this instance as the source of metric
    /// data.
    virtual_machine: VirtualMachine,

    /// The reset count for the relevant instance.
    run_count: virtual_machine::Reset,
}

impl ServerStatsInner {
    pub fn new(virtual_machine: VirtualMachine) -> Self {
        ServerStatsInner {
            virtual_machine,
            run_count: virtual_machine::Reset { datum: Default::default() },
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
    properties: &InstanceProperties,
    spec: &InstanceSpecV0,
) -> Option<KstatSampler> {
    let kstat_limit = usize::try_from(
        (u32::from(properties.vcpus) * KSTAT_LIMIT_PER_VCPU)
            + (spec.devices.network_devices.len() as u32 * SAMPLE_BUFFER),
    )
    .unwrap();

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

/// Track kstats required to publish network interface metrics for this instance.
#[cfg(any(test, not(target_os = "illumos")))]
pub(crate) async fn track_network_interface_kstats(
    log: &Logger,
    _: &KstatSampler,
    _: &InstanceProperties,
    _: NetworkInterfaceIds,
) {
    slog::error!(
        log,
        "network interface stats are not supported on this platform"
    );
}

/// Track kstats required to publish network interface metrics for this instance.
#[cfg(all(not(test), target_os = "illumos"))]
pub(crate) async fn track_network_interface_kstats(
    log: &Logger,
    sampler: &KstatSampler,
    properties: &InstanceProperties,
    interface_ids: NetworkInterfaceIds,
) {
    let nics = InstanceNetworkInterfaces::new(properties, interface_ids);
    let details = oximeter_instruments::kstat::CollectionDetails::never(
        NETWORK_INTERFACE_SAMPLE_INTERVAL,
    );
    let interface_id = nics.target.interface_id;
    if let Err(e) = sampler.add_target(nics, details).await {
        slog::error!(
            log,
            "failed to add network interface targets, \
            network interface stats will be unavailable";
            "network_interface_id" => %interface_id,
            "error" => ?e,
        );
    }
}

#[cfg(all(not(test), target_os = "illumos"))]
mod kstat_types {
    pub(crate) use kstat_rs::{Data, Kstat, Named, NamedData};
    pub(crate) use oximeter_instruments::kstat::{
        hrtime_to_utc, ConvertNamedData, Error, KstatList, KstatTarget,
    };
}

/// Mock the relevant subset of `kstat-rs` types needed for tests.
#[cfg(not(all(not(test), target_os = "illumos")))]
#[allow(dead_code, unused)]
mod kstat_types {
    use chrono::DateTime;
    use chrono::Utc;
    use oximeter::{Sample, Target};

    pub(crate) type KstatList<'a, 'k> =
        &'a [(DateTime<Utc>, Kstat<'k>, Data<'k>)];

    pub(crate) trait KstatTarget:
        Target + Send + Sync + 'static + std::fmt::Debug
    {
        /// Return true for any kstat you're interested in.
        fn interested(&self, kstat: &Kstat<'_>) -> bool;

        /// Convert from a kstat and its data to a list of samples.
        fn to_samples(
            &self,
            kstats: KstatList<'_, '_>,
        ) -> Result<Vec<Sample>, Error>;
    }

    #[derive(Debug, Clone)]
    pub(crate) enum Data<'a> {
        Named(Vec<Named<'a>>),
        Null,
    }

    #[derive(Debug, Clone)]
    pub(crate) enum NamedData<'a> {
        UInt32(u32),
        UInt64(u64),
        String(&'a str),
    }

    #[derive(Debug)]
    pub(crate) struct Kstat<'a> {
        pub ks_module: &'a str,
        pub ks_instance: i32,
        pub ks_name: &'a str,
        pub ks_snaptime: i64,
    }

    #[derive(Debug, Clone)]
    pub(crate) struct Named<'a> {
        pub name: &'a str,
        pub value: NamedData<'a>,
    }

    #[allow(unused)]
    pub(crate) trait ConvertNamedData {
        fn as_i32(&self) -> Result<i32, Error>;
        fn as_u32(&self) -> Result<u32, Error>;
        fn as_i64(&self) -> Result<i64, Error>;
        fn as_u64(&self) -> Result<u64, Error>;
    }

    impl<'a> ConvertNamedData for NamedData<'a> {
        fn as_i32(&self) -> Result<i32, Error> {
            unimplemented!()
        }

        fn as_u32(&self) -> Result<u32, Error> {
            if let NamedData::UInt32(x) = self {
                Ok(*x)
            } else {
                Err(Error::InvalidNamedData)
            }
        }

        fn as_i64(&self) -> Result<i64, Error> {
            unimplemented!()
        }

        fn as_u64(&self) -> Result<u64, Error> {
            if let NamedData::UInt64(x) = self {
                Ok(*x)
            } else {
                Err(Error::InvalidNamedData)
            }
        }
    }

    #[derive(thiserror::Error, Clone, Debug)]
    pub(crate) enum Error {
        #[error("No such kstat")]
        NoSuchKstat,
        #[error("Expected a named kstat")]
        ExpectedNamedKstat,
        #[error("Invalid named data")]
        InvalidNamedData,
        #[error("Sample error")]
        Sample(#[from] oximeter::MetricsError),
    }

    pub(crate) fn hrtime_to_utc(_: i64) -> Result<DateTime<Utc>, Error> {
        Ok(Utc::now())
    }
}
