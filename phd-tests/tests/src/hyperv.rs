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
                if check_clocksource {
                    let clocksource = target
                        .run_shell_command(
                            "cat /sys/devices/system/clocksource/clocksource0\
                            /current_clocksource",
                        )
                        .await
                        .unwrap();

                    assert_eq!(clocksource, "hyperv_clocksource_tsc_page");
                } else {
                    target.run_shell_command("").await.unwrap();
                }
            })
        },
    )
    .await?;

    // Only report a Passed result for this test if it actually managed to query
    // the clocksource. Note that if the clocksource can't be queried, but the
    // guest stops responding during the foregoing lifecycle test, the test will
    // fail (and report that result accordingly) before reaching this point.
    if !check_clocksource {
        phd_skip!("guest doesn't support querying clocksource through sysfs");
    }
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

            // Ideally, the guest and host readings would be taken
            // simultaneously, but in practice getting a guest timestamp itself
            // requires some work that itself takes time:
            //
            // 1. The framework needs to type the command into the guest
            // 2. The guest itself needs to run the command and print the
            //    result
            // 3. The framework needs to recognize a new command prompt, split
            //    off the result, and return it to the test case
            //
            // Snapshotting the host time here makes a bet that (3) is less
            // expensive than (1) and (2). This seems reasonable given that
            // executing a shell command involves both sending bytes to the
            // guest and waiting for them to be echoed, while waiting for the
            // result of an already-executed command just involves waiting for
            // the guest to print another prompt.
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

    // If the reference TSC is working properly, the guest should perceive that
    // time advances at roughly the same rate as on the host. To measure this,
    // take high-resolution time snapshots on both the host and guest at two
    // different times, then compare the host time and guest time differences to
    // see if they're approximately the same.
    //
    // The differences are approximate because there's no way to take a host and
    // guest time snapshot at exactly the same instant. The snapshotting logic
    // above tries to minimize this delta, but some error is still expected.
    // This test assumes that a measurement is "good" if the difference between
    // the host and guest readings, expressed as a percentage of the host time
    // delta, is less than this tolerance value.
    //
    // To further insulate itself from flakiness, this test takes multiple
    // readings and fails only if there are more "bad" (out-of-tolerance)
    // readings than "good" (in-tolerance) ones. This aims to ensure that one or
    // two bits of host machine weather don't cause the test to fail while still
    // detecting cases where the host and guest consistently disagree.
    const TOLERANCE: f64 = 0.005;

    // Take five readings here to produce four comparison pairs. This combines
    // with an extra cross-migration reading below to produce five "votes" for
    // whether the enlightenment is working as expected.
    let mut readings = vec![];
    for _ in 0..5 {
        readings.push(Reading::take_from(&vm).await?);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let mut good_diffs = 0;
    let mut bad_diffs = 0;
    for window in readings.as_slice().windows(2) {
        let first = &window[0];
        let second = &window[1];
        let diff_pct = second.compare_with_earlier(first);
        if diff_pct < TOLERANCE {
            good_diffs += 1;
        } else {
            bad_diffs += 1;
        }
    }

    // Take an extra pair of readings over a live migration. This also exercises
    // the time-data portion of the live migration protocol (albeit without
    // moving the VM to a different host machine).
    let mut target = ctx
        .spawn_successor_vm("hyperv_reference_tsc_elapsed_target", &vm, None)
        .await?;

    let before_migration = Reading::take_from(&vm).await?;
    target
        .migrate_from(&vm, Uuid::new_v4(), MigrationTimeout::default())
        .await?;

    let after_migration = Reading::take_from(&target).await?;
    let diff_pct = after_migration.compare_with_earlier(&before_migration);
    if diff_pct < TOLERANCE {
        good_diffs += 1;
    } else {
        bad_diffs += 1;
    }

    info!(in_tolerance_diffs = good_diffs, out_of_tolerance_diffs = bad_diffs);

    assert!(
        bad_diffs < good_diffs,
        "more out-of-tolerance time diffs ({}) than in-tolerance diffs ({}); \
        see test log for details",
        bad_diffs,
        good_diffs,
    );
}
