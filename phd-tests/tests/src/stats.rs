// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use oximeter::types::{ProducerResults, ProducerResultsItem, Sample};
use oximeter::{Datum, FieldValue};
use phd_testcase::*;
use tracing::{trace, warn};

// For convenience when comparing times below.
const NANOS_PER_SEC: f64 = 1_000_000_000.0;

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
enum VcpuState {
    Emulation,
    Run,
    Idle,
    Waiting,
}

#[derive(Default, Debug)]
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
#[derive(Debug)]
struct VirtualMachineMetrics {
    oldest_time: DateTime<Utc>,
    reset: Option<u64>,
    vcpus: HashMap<u32, VcpuUsageMetric>,
}

/// Collect a record of all metrics for a VM as of the time this function is called.
async fn vm_metrics_snapshot(
    sampler: &crate::stats::phd_framework::FakeOximeterContext,
) -> VirtualMachineMetrics {
    let min_metric_time = Utc::now();

    let metrics_check = move |producer_items| {
        producer_results_as_vm_metrics(producer_items)
            .filter(|metrics| metrics.oldest_time >= min_metric_time)
    };

    sampler.wait_for_propolis_stats(metrics_check).await
}

fn producer_results_as_vm_metrics(
    s: ProducerResults,
) -> Option<VirtualMachineMetrics> {
    let results = s;
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

    if metrics.vcpus.is_empty() {
        trace!("no vcpu metrics yet?");
        return None;
    }

    Some(metrics)
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
                    panic!("unexpected reset datum type: {:?}", datum);
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
                let state: VcpuState = if let FieldValue::String(state) =
                    &field.value
                {
                    VcpuState::from_str(state.as_ref()).unwrap_or_else(|_| {
                        panic!("unknown Oximeter vpcu state name: {}", state);
                    })
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

#[phd_testcase]
async fn instance_vcpu_stats(ctx: &Framework) {
    /// Allow as much as 20% measurement error for time comparisons in this
    /// test. When measuring active guest time, some guests (looking at you
    /// Windows) may have services that continue running in the time period
    /// where our test workload completes but we're still waiting for metrics;
    /// this means Oximeter can see more running time than we know we caused.
    /// When measuring guest idle time, these same idle services can result in
    /// the VM being less idle than our intended idling.
    ///
    /// "0.XX" here reflects an expectation that a system with no user-directed
    /// activity will actually be idle for XX% of a given time period. In
    /// practice this may be as low as 5% or less (many Linux guests), and as
    /// high as 12% in practice for Windows guests. Round up to 20% for some
    /// buffer.
    const TOLERANCE: f64 = 0.8;

    let mut env = ctx.environment_builder();
    env.metrics(Some(crate::stats::phd_framework::MetricsLocation::Local));

    let mut vm_config = ctx.vm_config_builder("instance_vcpu_stats");
    vm_config.cpus(1);

    let mut vm = ctx.spawn_vm(&vm_config, Some(&env)).await?;

    let sampler = vm.metrics_sampler().expect("metrics are enabled");
    vm.launch().await?;

    vm.wait_to_boot().await?;

    // From watching Linux guests, some services may be relatively active right
    // at and immediately after login. Wait a few seconds to try counting any
    // post-boot festivities as part of "baseline".
    vm.run_shell_command("sleep 20").await?;

    let start_metrics = vm_metrics_snapshot(&sampler).await;

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
    // busywork completes, but it's one fewer vms of nondeterminism.

    let run_start = std::time::SystemTime::now();
    vm.run_shell_command("i=0").await?;
    vm.run_shell_command("lim=2000000").await?;
    vm.run_shell_command("while [ $i -lt $lim ]; do i=$((i+1)); done").await?;
    let run_time = run_start.elapsed().expect("time goes forwards");
    tracing::warn!("measured run time {:?}", run_time);

    let now_metrics = vm_metrics_snapshot(&sampler).await;
    let total_run_window = run_start.elapsed().expect("time goes forwards");
    tracing::warn!("start_metrics: {:?}", start_metrics);
    tracing::warn!("now_metrics:   {:?}", now_metrics);

    let run_delta = (now_metrics.vcpu_state_total(&VcpuState::Run)
        - start_metrics.vcpu_state_total(&VcpuState::Run))
        as u128;

    // The guest should not have run longer than the total time we were
    // measuring it..
    assert!(run_delta < total_run_window.as_nanos());

    // Our measurement of how long the guest took should be pretty close to the
    // guest's measured running time. It won't be exact: the guest may have
    // services that continue in the period between the shell command completing
    // and a final metrics collection - it may be running for more time than we
    // intended.
    //
    // (Anecdotally the actual difference here depends on the guest, with
    // minimal Linux guests like Alpine being quite close with <1% differences
    // here.)
    let min_guest_run_delta = (run_time.as_nanos() as f64 * TOLERANCE) as u128;
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

    // Idle time boundaries are a little differnt than running time boundaries
    // because it's more difficult to stop counting to idle vCPU time than it is
    // to stop counting running vCPU time. Instead, the maximum amount of idling
    // time we might measure is however long it takes to get the initial kstat
    // readings, plus how long the idle time takes, plus however long it takes
    // to get final kstat readings. The miminum amount of idling time is
    // the time elapsed since just after the initial kstat readings.
    let max_idle_start = std::time::SystemTime::now();
    let idle_start_metrics = vm_metrics_snapshot(&sampler).await;
    let idle_start = std::time::SystemTime::now();
    vm.run_shell_command("sleep 10").await?;

    let now_metrics = vm_metrics_snapshot(&sampler).await;

    // The guest VM would continues to exist with its idle vCPU being accounted
    // by the kstats Oximeter samples. This means `vm_metrics_snapshot` could
    // introduce as much as a full Oximeter sample interval of additional idle
    // vCPU, and is we why wait to measure idle time until *after* getting new
    // Oximeter metrics.
    let max_idle_time = max_idle_start.elapsed().expect("time goes forwards");
    let idle_time = idle_start.elapsed().expect("time goes forwards");
    trace!("measured idle time {:?}", idle_time);

    let idle_delta = (now_metrics.vcpu_state_total(&VcpuState::Idle)
        - idle_start_metrics.vcpu_state_total(&VcpuState::Idle))
        as u128;

    // We've idled for at least 20 seconds. The guest may not be fully idle (its
    // OS is still running on its sole CPU, for example), so we test that the
    // guest was just mostly idle for the time period.
    assert!(
        idle_delta < max_idle_time.as_nanos(),
        "{} < {}",
        idle_delta as f64 / NANOS_PER_SEC,
        idle_time.as_nanos() as f64 / NANOS_PER_SEC
    );
    let min_guest_idle_delta =
        (idle_time.as_nanos() as f64 * TOLERANCE) as u128;
    assert!(
        idle_delta > min_guest_idle_delta,
        "{} > {}",
        idle_delta as f64 / NANOS_PER_SEC,
        min_guest_idle_delta as f64 / NANOS_PER_SEC
    );

    // The delta in vCPU `run` time should be negligible. We've run one shell
    // command which in turn just idled. In reality, if the guest has idle
    // processes running even sitting at an empty prompt, assume there is up to
    // THRESHOLD activity happening anyway. This is another threshold that
    // varies based on guest OS type.
    let run_delta = (now_metrics.vcpu_state_total(&VcpuState::Run)
        - idle_start_metrics.vcpu_state_total(&VcpuState::Run))
        as u128;
    let idle_delta = (now_metrics.vcpu_state_total(&VcpuState::Idle)
        - idle_start_metrics.vcpu_state_total(&VcpuState::Idle))
        as u128;
    assert!(run_delta < (idle_delta as f64 * (1.0 - TOLERANCE)) as u128);

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
    if vm.guest_os_kind().is_linux() {
        // Unfortunately, the above is currently too optimistic in the general
        // case of arbitrary guest OSes - if a guest OS has idle services, and
        // those idle services involve checking the current time, and the guest
        // has determined the TSC is unreliable, we may count substantial
        // emulation time due to emulating guest accesses to the ACPI PM timer.
        //
        // Linux guests are known to not (currently?) consult the current time
        // if fully idle, so we can be more precise about emulation time
        // assertions.
        assert!(
            full_emul_delta < Duration::from_millis(100).as_nanos(),
            "full emul delta was above threshold: {} > {}",
            full_emul_delta,
            Duration::from_millis(100).as_nanos()
        );
    } else {
        warn!(
            "guest OS may cause substantial emulation time due to benign \
               factors outside our control; skipping emulation stat check"
        );
    }

    // Waiting is a similar but more constrained situation as `emul`: time when
    // the vCPU was runnable but not *actually* running. This should be a very
    // short duration, and on my workstation this is around 400 microseconds.
    // Again, test against a significantly larger threshold in case CI is
    // extremely slow.
    assert!(
        full_waiting_delta < Duration::from_millis(20).as_nanos(),
        "full waiting delta was above threshold: {} > {}",
        full_waiting_delta,
        Duration::from_millis(20).as_nanos()
    );

    trace!("run: {}", full_run_delta);
    trace!("idle: {}", full_idle_delta);
    trace!("waiting: {}", full_waiting_delta);
    trace!("emul: {}", full_emul_delta);
}
