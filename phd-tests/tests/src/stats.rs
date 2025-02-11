// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use phd_testcase::*;
use tracing::trace;
use uuid::Uuid;

use chrono::{DateTime, Utc};
use dropshot::endpoint;
use dropshot::ApiDescription;
use dropshot::ConfigDropshot;
use dropshot::HttpError;
use dropshot::HttpResponseCreated;
use dropshot::HttpServer;
use dropshot::HttpServerStarter;
use dropshot::RequestContext;
use dropshot::TypedBody;
use omicron_common::api::internal::nexus::ProducerEndpoint;
use omicron_common::api::internal::nexus::ProducerKind;
use omicron_common::api::internal::nexus::ProducerRegistrationResponse;
use oximeter::types::{ProducerResults, ProducerResultsItem, Sample};
use oximeter::{Datum, FieldValue};
use slog::Drain;
use slog::Logger;

fn test_logger() -> Logger {
    let dec = slog_term::PlainSyncDecorator::new(slog_term::TestStdoutWriter);
    let drain = slog_term::FullFormat::new(dec).build().fuse();
    let log = Logger::root(drain, slog::o!("component" => "fake-cleanup-task"));
    log
}

// Re-registration interval for tests. A long value here helps avoid log spew
// from Oximeter, which will re-register after about 1/6th of this interval
// elapses.
const INTERVAL: Duration = Duration::from_secs(300);

// For convenience when comparing times below.
const NANOS_PER_SEC: f64 = 1_000_000_000.0;

struct PropolisOximeterSampler {
    addr: std::net::SocketAddr,
    uuid: Uuid,
}

struct FakeNexusContext {
    sampler: Arc<Mutex<Option<PropolisOximeterSampler>>>,
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
enum VcpuState {
    Emulation,
    Run,
    Idle,
    Waiting,
}

impl VcpuState {
    fn from_oximeter_state_name(name: &str) -> Self {
        match name {
            "emulation" => VcpuState::Emulation,
            "run" => VcpuState::Run,
            "idle" => VcpuState::Idle,
            "waiting" => VcpuState::Waiting,
            other => {
                panic!("unknown Oximeter vpcu state name: {}", other);
            }
        }
    }
}

#[derive(Default)]
struct VcpuUsageMetric {
    metrics: HashMap<VcpuState, u64>,
}

/// A collection of the stats produced by `propolis-server`'s Oximeter producer.
///
/// Oximeter producers produce a series of lists of samples, where each list
/// of samples is conceptually distinct but may still be interesting to
/// tset. In `propolis-server`, the first list of samples will be
/// `virtual_machine:vcpu_usage`, which may be blank of kstats have not been
/// sampled since the last producer poll. The second list of samples
/// will be `virtual_machine:reset`.
///
/// `VirtualMachineMetrics` collects these all back together into a single view
/// to test against. See [`VirtualMachineMetrics::add_producer_result`] as the
/// means to accumulate samples into this struct.
struct VirtualMachineMetrics {
    oldest_time: DateTime<Utc>,
    reset: Option<u64>,
    vcpus: HashMap<u32, VcpuUsageMetric>,
}

impl VirtualMachineMetrics {
    fn vcpu_state_total(&self, state: &VcpuState) -> u64 {
        self.vcpus
            .values()
            .fold(0, |total, vcpu_usage| total + vcpu_usage.metrics[state])
    }

    fn update_metric_times(&mut self, metric_time: DateTime<Utc>) {
        self.oldest_time = std::cmp::min(self.oldest_time, metric_time);
    }

    /// Integrate a list of samples into this collection of virtual machine
    /// metrics.
    fn add_producer_result(&mut self, samples: &[Sample]) {
        let mut samples_by_metric = HashMap::new();

        for sample in samples {
            let name = sample.timeseries_name.to_owned();
            let fields = sample.sorted_metric_fields().to_owned();
            let collection: &mut Vec<Sample> =
                samples_by_metric.entry((name, fields)).or_default();

            collection.push(sample.clone());
        }

        for v in samples_by_metric.values_mut() {
            v.sort_by_key(|s| s.measurement.timestamp());
        }

        for ((name, fields), samples) in samples_by_metric {
            let last_sample = samples.last().expect("at least one sample");
            if name == "virtual_machine:reset" {
                assert!(
                    self.reset.is_none(),
                    "multiple virtual_machine:reset measurements for a \
                     single Propolis?"
                );

                let datum = last_sample.measurement.datum();
                let amount = if let Datum::CumulativeU64(amount) = datum {
                    amount.value()
                } else {
                    panic!("unexpected reset value type");
                };
                self.reset = Some(amount);
                self.update_metric_times(last_sample.measurement.timestamp());
            } else if name == "virtual_machine:vcpu_usage" {
                let datum = last_sample.measurement.datum();
                let amount = if let Datum::CumulativeU64(amount) = datum {
                    amount.value()
                } else {
                    panic!("unexpected vcpu_usage datum type: {:?}", datum);
                };
                let field = &fields["state"];
                let state: VcpuState =
                    if let FieldValue::String(state) = &field.value {
                        VcpuState::from_oximeter_state_name(state.as_ref())
                    } else {
                        panic!("unknown vcpu state datum type: {:?}", field);
                    };
                let field = &fields["vcpu_id"];
                let vcpu_id = if let FieldValue::U32(vcpu_id) = field.value {
                    vcpu_id
                } else {
                    panic!("unknown vcpu id datum type: {:?}", field);
                };
                let vcpu_metrics = self.vcpus.entry(vcpu_id).or_default();
                if vcpu_metrics.metrics.contains_key(&state) {
                    panic!(
                        "vcpu {} state {:?} has duplicate metric {:?}",
                        vcpu_id, state, last_sample
                    );
                }
                trace!(
                    "recorded cpu {} state {:?} = {} at {}",
                    vcpu_id,
                    state,
                    amount,
                    last_sample.measurement.timestamp()
                );
                vcpu_metrics.metrics.insert(state, amount);
                self.update_metric_times(last_sample.measurement.timestamp());
            }
        }
    }
}

impl FakeNexusContext {
    fn new() -> Self {
        Self { sampler: Arc::new(Mutex::new(None)) }
    }

    fn set_producer_info(&self, info: ProducerEndpoint) {
        assert_eq!(info.kind, ProducerKind::Instance);
        *self.sampler.lock().unwrap() =
            Some(PropolisOximeterSampler { addr: info.address, uuid: info.id });
    }

    async fn wait_for_producer(&self) {
        loop {
            {
                let sampler = self.sampler.lock().unwrap();
                if sampler.is_some() {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Sample Propolis' Oximeter metrics, waiting up to a few seconds so that
    /// all measurements are from the time this function was called or later.
    async fn wait_for_propolis_stats(&self) -> VirtualMachineMetrics {
        let retry_delay = Duration::from_millis(1000);
        let max_wait = Duration::from_millis(10000);
        let wait_start = std::time::SystemTime::now();

        let min_metric_time = Utc::now();

        while wait_start.elapsed().expect("time goes forward") < max_wait {
            if let Some(metrics) = self.sample_propolis_stats().await {
                if metrics.oldest_time >= min_metric_time {
                    return metrics;
                }
            }

            tokio::time::sleep(retry_delay).await;
        }

        panic!(
            "propolis-server Oximeter stats unavailable? waited {:?}",
            max_wait
        );
    }

    /// Sample Propolis' Oximeter metrics, including the timestamp of the oldest
    /// metric reflected in the sample.
    ///
    /// Returns `None` for some kinds of incomplete stats or when no stats are
    /// available at all.
    async fn sample_propolis_stats(&self) -> Option<VirtualMachineMetrics> {
        let metrics_url = {
            let sampler = self.sampler.lock().unwrap();
            let stats = sampler.as_ref().expect("stats url info exists");
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
        let results =
            res.json::<ProducerResults>().await.expect("can deserialize");

        let mut metrics = VirtualMachineMetrics {
            oldest_time: Utc::now(),
            reset: None,
            vcpus: HashMap::new(),
        };

        for result in results {
            match result {
                ProducerResultsItem::Ok(samples) => {
                    metrics.add_producer_result(&samples);
                }
                ProducerResultsItem::Err(e) => {
                    panic!("ProducerResultsItem error: {}", e);
                }
            }
        }

        if metrics.vcpus.len() == 0 {
            trace!("no vcpu metrics yet?");
            return None;
        }

        Some(metrics)
    }
}

// Stub functionality for our fake Nexus that test Oximeter produces
// (`propolis-server`) will register with.
#[endpoint {
    method = POST,
    path = "/metrics/producers",
}]
async fn register_producer(
    rqctx: RequestContext<FakeNexusContext>,
    producer_info: TypedBody<ProducerEndpoint>,
) -> Result<HttpResponseCreated<ProducerRegistrationResponse>, HttpError> {
    let info = producer_info.into_inner();
    trace!(?info, "producer registration");
    rqctx.context().set_producer_info(info);

    Ok(HttpResponseCreated(ProducerRegistrationResponse {
        lease_duration: INTERVAL,
    }))
}

// Start a Dropshot server mocking the Nexus registration endpoint.
fn spawn_fake_nexus_server() -> HttpServer<FakeNexusContext> {
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
        FakeNexusContext::new(),
        &log,
    )
    .expect("Expected to start Dropshot server")
    .start();

    slog::info!(
        log,
        "fake nexus test server listening";
        "address" => ?server.local_addr(),
    );

    server
}

#[phd_testcase]
async fn instance_vcpu_stats(ctx: &Framework) {
    let fake_nexus = spawn_fake_nexus_server();

    let mut env = ctx.environment_builder();
    env.metrics_addr(Some(fake_nexus.local_addr()));

    let mut vm_config = ctx.vm_config_builder("instance_vcpu_stats");
    vm_config.cpus(1);

    let mut source = ctx.spawn_vm(&vm_config, Some(&env)).await?;

    source.launch().await?;

    fake_nexus.app_private().wait_for_producer().await;
    source.wait_to_boot().await?;

    // From watching Linux guests, some services may be relatively active right
    // at and immediately after login. Wait a few seconds to try counting any
    // post-boot festivities as part of "baseline".
    source.run_shell_command("sleep 10").await?;

    let start_metrics =
        fake_nexus.app_private().wait_for_propolis_stats().await;

    // Measure a specific amount of time with guest vCPUs in the "run" state.
    //
    // We measure the "run" state using some fixed-size busywork because we
    // can't simply say "run for 5 seconds please" - if we did, a combination of
    // host OS or guest OS may leave the process timing itself descheduled for
    // some or all of that time, so we could end up with substantially less than
    // 5 seconds of execution and a flaky test as a result.
    //
    // Instead, run some busywork, time how long that took on the host OS, then
    // know the guest OS should have spent around that long running. This still
    // relies us measuring the completion time relatively quickly after the
    // busywork completes, but it's one fewer sources of nondeterminism.

    let run_start = std::time::SystemTime::now();
    source.run_shell_command("i=0").await?;
    source.run_shell_command("lim=2000000").await?;
    source
        .run_shell_command("while [ $i -lt $lim ]; do i=$((i+1)); done")
        .await?;
    let run_time = run_start.elapsed().expect("time goes forwards");
    trace!("measured run time {:?}", run_time);

    let now_metrics = fake_nexus.app_private().wait_for_propolis_stats().await;

    let run_delta = (now_metrics.vcpu_state_total(&VcpuState::Run)
        - start_metrics.vcpu_state_total(&VcpuState::Run))
        as u128;

    // The guest should not have run longer than we were running a shell command
    // in the guest..
    assert!(run_delta < run_time.as_nanos());

    // Our measurement of how long the guest took should be pretty close to the
    // guest's measured running time. Check only that the guest ran for at least
    // 90% of the time we measured it running because we're woken strictly after
    // the guest completed its work - we know we've overcounted some.
    //
    // (Anecdotally the actual difference here on a responsive test system is
    // closer to 10ms, or <1% of expected runtime. Lots of margin for error on a
    // very busy CI system.)
    let min_guest_run_delta = (run_time.as_nanos() as f64 * 0.9) as u128;
    assert!(
        run_delta > min_guest_run_delta,
        "{} > {}",
        run_delta as f64 / NANOS_PER_SEC,
        min_guest_run_delta as f64 / NANOS_PER_SEC
    );

    // VM vCPU stats are sampled roughly every five seconds, which means the
    // minimum granularity of `run + idle + waiting + emul` is also roughly
    // units of 5 seconds. There could be one or two sample intervals between
    // `start_metrics` and `now_metrics` depending on how long it took to get
    // from starting the Oximeter producer to actually sampling.
    //
    // This is to say: there isn't a strong statement that we can make about
    // idle time at this point other than that it is probably around a large
    // enough value to fill the total time out to a mutiple of 5 seconds.
    //
    // The guesswork to validate that doesn't seem great in the face of
    // variable-time CI. We'll validate idle time measurements separately,
    // below.
    let idle_start_metrics =
        fake_nexus.app_private().wait_for_propolis_stats().await;
    let idle_start = std::time::SystemTime::now();
    source.run_shell_command("sleep 20").await?;

    let now_metrics = fake_nexus.app_private().wait_for_propolis_stats().await;

    // The guest VM would continues to exist with its idle vCPU being accounted
    // by the kstats Oximeter samples. This means `wait_for_propolis_stats`
    // could introduce as much as a full Oximeter sample interval of additional
    // idle vCPU, and is we why wait to measure idle time until *after* getting
    // new Oximeter metrics.
    let idle_time = idle_start.elapsed().expect("time goes forwards");
    trace!("measured idle time {:?}", idle_time);

    let idle_delta = (now_metrics.vcpu_state_total(&VcpuState::Idle)
        - idle_start_metrics.vcpu_state_total(&VcpuState::Idle))
        as u128;

    // We've idled for at least 20 seconds. The guest may not be fully idle (its
    // OS is still running on its sole CPU, for example), so we test that the
    // guest was just mostly idle for the time period.
    let min_guest_idle_delta = (idle_time.as_nanos() as f64 * 0.9) as u128;
    assert!(
        idle_delta < idle_time.as_nanos(),
        "{} < {}",
        idle_delta as f64 / NANOS_PER_SEC,
        idle_time.as_nanos() as f64 / NANOS_PER_SEC
    );
    assert!(
        idle_delta > min_guest_idle_delta,
        "{} > {}",
        idle_delta as f64 / NANOS_PER_SEC,
        min_guest_idle_delta as f64 / NANOS_PER_SEC
    );

    // The delta in vCPU `run` time should be negligible. We've run one shell
    // command which in turn just idled.
    let run_delta = (now_metrics.vcpu_state_total(&VcpuState::Run)
        - idle_start_metrics.vcpu_state_total(&VcpuState::Run))
        as u128;
    assert!(run_delta < Duration::from_millis(100).as_nanos());

    let full_run_delta = (now_metrics.vcpu_state_total(&VcpuState::Run)
        - start_metrics.vcpu_state_total(&VcpuState::Run))
        as u128;

    let full_idle_delta = (now_metrics.vcpu_state_total(&VcpuState::Idle)
        - start_metrics.vcpu_state_total(&VcpuState::Idle))
        as u128;

    let full_waiting_delta = (now_metrics.vcpu_state_total(&VcpuState::Waiting)
        - start_metrics.vcpu_state_total(&VcpuState::Waiting))
        as u128;

    let full_emul_delta = (now_metrics.vcpu_state_total(&VcpuState::Emulation)
        - start_metrics.vcpu_state_total(&VcpuState::Emulation))
        as u128;

    // Pick 100ms as a comically high upper bound for how much time might have
    // been spent emulating instructions on the guest's behalf. Anecdotally the
    // this is on the order of 8ms between the two samples. This should be very
    // low; the workload is almost entirely guest user mode execution.
    assert!(full_emul_delta < Duration::from_millis(100).as_nanos());

    // Waiting is a similar but more constrained situation as `emul`: time when
    // the vCPU was runnable but not *actually* running. This should be a very
    // short duration, and on my workstation this is around 400 microseconds.
    // Again, test against a significantly larger threshold in case CI is
    // extremely slow.
    assert!(full_waiting_delta < Duration::from_millis(20).as_nanos());

    trace!("run: {}", full_run_delta);
    trace!("idle: {}", full_idle_delta);
    trace!("waiting: {}", full_waiting_delta);
    trace!("emul: {}", full_emul_delta);
}
