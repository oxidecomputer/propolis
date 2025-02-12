// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::{Duration, Instant};

use phd_framework::{
    artifacts, lifecycle::Action, test_vm::MigrationTimeout, TestVm,
};
use phd_testcase::*;
use propolis_client::types::HyperVFeatureFlag;
use tracing::{info, warn};
use uuid::Uuid;

/// Attempts to see if the guest has detected Hyper-V support. This is
/// best-effort, since not all PHD guest images contain in-box tools that
/// display the current hypervisor vendor.
///
/// NOTE: If the guest lacks a facility to check the hypervisor vendor, this
/// routine logs a warning but does not return a "Skipped" result. This allows
/// the smoke tests to return a Pass result to show that they exercised VM
/// startup and shutdown with Hyper-V emulation enabled.
async fn guest_detect_hyperv(vm: &TestVm) -> anyhow::Result<()> {
    if vm.guest_os_kind().is_linux() {
        // Many Linux distros come with systemd installed out of the box. On
        // these distros, it's easiest to use `systemd-detect-virt` to determine
        // whether the guest thinks it's running on a Hyper-V-compatible
        // hypervisor. (Whether any actual enlightenments are enabled is another
        // story, but those can often be detected by other means.)
        let out = vm.run_shell_command("systemd-detect-virt").await?;
        if out.contains("systemd-detect-virt: not found") {
            warn!(
                "guest doesn't support systemd-detect-virt, can't verify it \
                detected Hyper-V support"
            );
        } else {
            assert_eq!(out, "microsoft");
        }
    } else if vm.guest_os_kind().is_windows() {
        // Windows is good about giving signals that it's running in a Hyper-V
        // *root partition*, but offers no clear signal as to whether it has
        // detected a Hyper-V host when it's running as a non-root guest. (There
        // are methods for detecting whether Windows is running as a guest, but
        // these don't identify the detected hypervisor type.)
        warn!("running on Windows, can't verify it detected Hyper-V support");
    }

    Ok(())
}

#[phd_testcase]
async fn hyperv_smoke_test(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("hyperv_smoke_test");
    cfg.guest_hv_interface(
        propolis_client::types::GuestHypervisorInterface::HyperV {
            features: vec![],
        },
    );
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    guest_detect_hyperv(&vm).await?;
}

#[phd_testcase]
async fn hyperv_lifecycle_test(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("hyperv_lifecycle_test");
    cfg.guest_hv_interface(
        propolis_client::types::GuestHypervisorInterface::HyperV {
            features: vec![],
        },
    );
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    ctx.lifecycle_test(
        vm,
        &[
            Action::Reset,
            Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT),
        ],
        |target: &TestVm| {
            Box::pin(async {
                guest_detect_hyperv(target).await.unwrap();
            })
        },
    )
    .await?;
}

#[phd_testcase]
async fn hyperv_reference_tsc_clocksource_test(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("hyperv_reference_tsc_test");
    cfg.guest_hv_interface(
        propolis_client::types::GuestHypervisorInterface::HyperV {
            features: vec![HyperVFeatureFlag::ReferenceTsc],
        },
    );
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let clocksource = vm
        .run_shell_command(
            "cat /sys/devices/system/clocksource/clocksource0\
                /current_clocksource",
        )
        .await?;

    let check_clocksource = !clocksource.ends_with("No such file or directory");
    if check_clocksource {
        assert_eq!(clocksource, "hyperv_clocksource_tsc_page");
    } else {
        warn!("guest doesn't support querying clocksource through sysfs");
    }

    // Migrate to a new VM and make sure the clocksource is kept intact. If
    // clocksource queries aren't supported for this guest, poke the serial
    // console anyway just to make sure the guest remains operable.
    ctx.lifecycle_test(
        vm,
        &[
            Action::Reset,
            Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT),
        ],
        |target: &TestVm| {
            Box::pin(async move {
                if !check_clocksource {
                    let echo = target
                        .run_shell_command(
                            "echo clocksource queries not supported",
                        )
                        .await
                        .unwrap();

                    assert_eq!(echo, "clocksource queries not supported");
                } else {
                    let clocksource = target
                        .run_shell_command(
                            "cat /sys/devices/system/clocksource/clocksource0\
                        /current_clocksource",
                        )
                        .await
                        .unwrap();

                    assert_eq!(clocksource, "hyperv_clocksource_tsc_page");
                }
            })
        },
    )
    .await?;
}

#[phd_testcase]
async fn hyperv_reference_tsc_elapsed_time_test(ctx: &Framework) {
    if ctx.default_guest_os_kind().await?.is_windows() {
        phd_skip!("test requires a guest with /proc/timer_list in procfs");
    }

    let mut cfg = ctx.vm_config_builder("hyperv_reference_tsc_elapsed_test");
    cfg.guest_hv_interface(
        propolis_client::types::GuestHypervisorInterface::HyperV {
            features: vec![HyperVFeatureFlag::ReferenceTsc],
        },
    );
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    #[derive(Debug)]
    struct Reading {
        taken_at: Instant,
        guest_ns: u64,
    }

    impl Reading {
        async fn take_from(vm: &TestVm) -> anyhow::Result<Self> {
            let cmd =
                "cat /proc/timer_list | grep \"now at\" | awk '{ print $3 }'";
            let guest_ns = vm.run_shell_command(cmd).await?.parse::<u64>()?;

            // It's important to minimize the time between the host and guest
            // readings to avoid introducing skew when comparing sets of
            // readings. Capturing the host timestamp after the guest time
            // excludes the time needed to send bytes to the guest serial potr
            // and wait for them to be echoed from this delta.
            let taken_at = Instant::now();
            Ok(Self { taken_at, guest_ns })
        }

        /// Compares `self` with an earlier reading, `other`, and returns the
        /// difference between the measured elapsed time on the host and the
        /// measured elapsed time on the guest, expressed as a percentage of the
        /// measured elapsed time on the host.
        fn compare_with_earlier(&self, other: &Reading) -> f64 {
            let host_delta_ns =
                i64::try_from((self.taken_at - other.taken_at).as_nanos())
                    .expect("host delta is small enough to fit in an i64");

            let guest_delta_ns = i64::try_from(self.guest_ns - other.guest_ns)
                .expect("guest delta is small enough to fit in an i64");

            let diff = (host_delta_ns - guest_delta_ns).unsigned_abs();
            let diff_pct = (diff as f64) / (host_delta_ns as f64);

            info!(
                before = ?other,
                after = ?self,
                host_delta_ns,
                guest_delta_ns,
                diff,
                diff_pct,
                "compared time readings"
            );

            diff_pct
        }
    }

    // The logic below takes pairs of time readings on the host and guest and
    // compares them to check that the amount of elapsed time perceived by the
    // guest is roughly equivalent to the "real" elapsed time on the host. The
    // host and guest measurements can't be taken atomically, so some difference
    // between these readings is expected (especially because the guest shell
    // command needs to print its result to the serial port, and the harness has
    // to read it).
    //
    // Accept a measurement error of up to 1% of the elapsed time between host
    // readings. This is an extremely generous tolerance. It is chosen primarily
    // to avoid flakiness while still checking for gross errors in the
    // construction of the TSC page (e.g. mis-shifting the TSC scaling factor
    // so that it's off by a factor of 2).
    const TOLERANCE: f64 = 0.01;

    let mut readings = vec![];
    for _ in 0..5 {
        readings.push(Reading::take_from(&vm).await?);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    for window in readings.as_slice().windows(2) {
        let first = &window[0];
        let second = &window[1];
        let diff_pct = second.compare_with_earlier(first);
        assert!(
            diff_pct < TOLERANCE,
            "time readings {first:?} and {second:?} differ by more than a
            factor of {TOLERANCE}"
        );
    }

    // For good measure, also take readings over a live migration; this also
    // helps to verify the time-adjustment portions of the migration protocol.
    let mut target = ctx
        .spawn_successor_vm("hyperv_reference_tsc_elapsed_target", &vm, None)
        .await?;

    let before_migration = Reading::take_from(&vm).await?;
    target
        .migrate_from(&vm, Uuid::new_v4(), MigrationTimeout::default())
        .await?;

    let after_migration = Reading::take_from(&target).await?;
    let diff_pct = after_migration.compare_with_earlier(&before_migration);
    assert!(
        diff_pct < TOLERANCE,
        "time readings {:?} and {:?} differ by more than a
        factor of {TOLERANCE}",
        before_migration,
        after_migration
    );
}
