use std::net::SocketAddr;
use std::time::Duration;

use dropshot::{
    endpoint, ApiDescription, ConfigDropshot, HttpError, HttpResponseCreated,
    HttpServer, HttpServerStarter, RequestContext, TypedBody,
};
use omicron_common::api::internal::nexus::{
    ProducerEndpoint, ProducerKind, ProducerRegistrationResponse,
};
use oximeter::types::ProducerResults;
use slog::{Drain, Logger};
use tokio::sync::watch;
use tracing::trace;
use uuid::Uuid;

// Re-registration interval for tests. A long value here helps avoid log spew
// from Oximeter, which will re-register after about 1/6th of this interval
// elapses.
const INTERVAL: Duration = Duration::from_secs(300);

fn test_logger() -> Logger {
    let dec = slog_term::PlainSyncDecorator::new(slog_term::TestStdoutWriter);
    let drain = slog_term::FullFormat::new(dec).build().fuse();
    Logger::root(drain, slog::o!("component" => "fake-cleanup-task"))
}

struct PropolisOximeterSampler {
    addr: std::net::SocketAddr,
    uuid: Uuid,
}

pub(crate) struct FakeOximeterServer {
    server: HttpServer<FakeOximeterServerState>,
}

pub(crate) struct FakeOximeterServerState {
    sampler_sender: watch::Sender<Option<PropolisOximeterSampler>>,
    sampler: watch::Receiver<Option<PropolisOximeterSampler>>,
}

impl FakeOximeterServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    pub fn sampler(&self) -> FakeOximeterContext {
        FakeOximeterContext {
            sampler: self.server.app_private().sampler.clone(),
        }
    }
}

pub struct FakeOximeterContext {
    sampler: watch::Receiver<Option<PropolisOximeterSampler>>,
}

impl FakeOximeterServerState {
    fn new() -> Self {
        let (tx, rx) = watch::channel(None);

        Self { sampler_sender: tx, sampler: rx }
    }

    async fn set_producer_info(&self, info: ProducerEndpoint) {
        // Just don't know what to do with other ProducerKinds, if or when we'll
        // see them here..
        assert_eq!(info.kind, ProducerKind::Instance);

        let new_sampler =
            PropolisOximeterSampler { addr: info.address, uuid: info.id };

        // There should always be at least one Receiver on the channel since we
        // hold one in `self`.
        self.sampler_sender
            .send(Some(new_sampler))
            .expect("channel is subscribed");
    }
}

impl FakeOximeterContext {
    /// Sample Propolis' Oximeter metrics, taking some function that determines
    /// if a sample is satisfactory for the caller to proceed with.
    ///
    /// `wait_for_propolis_stats` will poll the corresponding Oximeter producer
    /// and call `f` with each returned set of results.
    ///
    /// Panics if `f` does not return `Some` after some number of retries and
    /// `ProducerResults` updates.
    pub async fn wait_for_propolis_stats<U>(
        &self,
        f: impl Fn(ProducerResults) -> Option<U>,
    ) -> U {
        let result = backoff::future::retry(
            backoff::ExponentialBackoff {
                max_interval: Duration::from_secs(1),
                max_elapsed_time: Some(Duration::from_secs(10)),
                ..Default::default()
            },
            || async {
                let producer_results = self.sample_propolis_stats().await
                    .map_err(|e| backoff::Error::transient(e))?;

                if let Some(metrics) = f(producer_results) {
                    Ok(metrics)
                } else {
                    Err(backoff::Error::transient(anyhow::anyhow!(
                        "full metrics sample not available or fresh enough (yet?)"
                    )))
                }
            },
        )
        .await;

        result.expect("propolis-server Oximeter stats should become available")
    }

    /// Sample Propolis' Oximeter metrics, including the timestamp of the oldest
    /// metric reflected in the sample.
    ///
    /// Returns `None` for some kinds of incomplete stats or when no stats are
    /// available at all.
    async fn sample_propolis_stats(
        &self,
    ) -> Result<ProducerResults, anyhow::Error> {
        let metrics_url = {
            self.sampler
                .clone()
                .wait_for(Option::is_some)
                .await
                .expect("can recv");
            let sampler = self.sampler.borrow();
            let stats = sampler.as_ref().expect("sampler does not become None");
            format!("http://{}/{}", stats.addr, stats.uuid)
        };
        let res = reqwest::Client::new()
            .get(metrics_url)
            .send()
            .await
            .expect("can send oximeter stats request");
        assert!(
            res.status().is_success(),
            "failed to fetch stats from propolis-server"
        );
        trace!(?res, "got stats response");
        Ok(res.json::<ProducerResults>().await?)
        /*
        let results =
            res.json::<ProducerResults>().await.expect("can deserialize");
            */
    }
}

// Stub functionality for our fake Nexus that test Oximeter produces
// (`propolis-server`) will register with.
#[endpoint {
    method = POST,
    path = "/metrics/producers",
}]
async fn register_producer(
    rqctx: RequestContext<FakeOximeterServerState>,
    producer_info: TypedBody<ProducerEndpoint>,
) -> Result<HttpResponseCreated<ProducerRegistrationResponse>, HttpError> {
    let info = producer_info.into_inner();
    trace!(?info, "producer registration");
    rqctx.context().set_producer_info(info).await;

    Ok(HttpResponseCreated(ProducerRegistrationResponse {
        lease_duration: INTERVAL,
    }))
}

// Start a Dropshot server mocking the Oximeter registration endpoint we would
// expect from Nexus.
pub fn spawn_fake_oximeter_server() -> FakeOximeterServer {
    let log = test_logger();

    let mut api = ApiDescription::new();
    api.register(register_producer).expect("Expected to register endpoint");
    let server = HttpServerStarter::new(
        &ConfigDropshot {
            bind_address: "[::1]:0".parse().unwrap(),
            request_body_max_bytes: 2048,
            ..Default::default()
        },
        api,
        FakeOximeterServerState::new(),
        &log,
    )
    .expect("Expected to start Dropshot server")
    .start();

    slog::info!(
        log,
        "fake nexus test server listening";
        "address" => ?server.local_addr(),
    );

    FakeOximeterServer { server }
}
