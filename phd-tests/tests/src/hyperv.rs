// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::{Duration, Instant};

use phd_framework::{
    artifacts, lifecycle::Action, test_vm::MigrationTimeout, TestVm,
};
use phd_testcase::*;
use propolis_client::instance_spec::{
    GuestHypervisorInterface, HyperVFeatureFlag,
};
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
        // One might imagine we could simply use `systemd-detect-virt` to check
        // hypervisor information here, but it's not present out of the box on
        // Alpine. In the interest of exercising Hyper-V in typical CI runs on a
        // standard Alpine image, detect Hyper-V in a.. worse but reliable way:
        // looking for relevant logs in dmesg. This should work for all Linuxes
        // from later than May-ish 2010 (>2.6.34 or so). If we don't see Hyper-V
        // reported in dmesg either it's genuinely not detected, it's a very old
        // Linux, or it's a new Linux and dmesg text has changed.
        const HV_TEXT: &str = "Hypervisor detected: Microsoft Hyper-V";

        // No "sudo" here because Alpine doesn't have sudo; for Linux tests we
        // expect to run test commands as root anyway.
        vm.run_shell_command(&format!("dmesg | grep \"{HV_TEXT}\"")).await?;
    } else if vm.guest_os_kind().is_windows() {
        // Windows is good about giving signals that it's running in a Hyper-V
        // *root partition*, but offers no clear signal as to whether it has
        // detected a Hyper-V host when it's running as a non-root guest. (There
        // are methods for detecting whether Windows is running as a guest, but
        // these don't identify the detected hypervisor type.)
        warn!("running on Windows, can't verify it detected Hyper-V support");
    } else {
        warn!("unknown guest type, can't verify it detected Hyper-V support");
    }

    Ok(())
}

#[phd_testcase]
async fn hyperv_smoke_test(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("hyperv_smoke_test");
    cfg.guest_hv_interface(GuestHypervisorInterface::HyperV {
        features: Default::default(),
    });
    // For reasons absolutely indecipherable to me, Alpine (3.16, kernel
    // 5.15.41-0-virt) seems to lose some early dmesg lines if booted with less
    // than four vCPUs. Among the early dmesg lines are `Hypervisor detected:
    // Microsoft Hyper-V` that we look for as confirmation that Linux knows
    // there's a Hyper-V-like hypervisor present.
    //
    // I'd love to debug exactly why this is relevant to the contents of dmesg
    // (the remaining log is identical and it doesn't seem that the ring buffer
    // is full), but I really can't justify the time!
    cfg.cpus(4);
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    guest_detect_hyperv(&vm).await?;
}

#[phd_testcase]
async fn hyperv_lifecycle_test(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("hyperv_lifecycle_test");
    cfg.guest_hv_interface(GuestHypervisorInterface::HyperV {
        features: Default::default(),
    });
    // Spooky load-bearing vCPU count to preserve dmesg log lines. See the
    // comment in `hyperv_smoke_test`.
    cfg.cpus(4);
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
    cfg.guest_hv_interface(GuestHypervisorInterface::HyperV {
        features: [HyperVFeatureFlag::ReferenceTsc].into_iter().collect(),
    });
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

    /// Checks that time is advancing at roughly the correct rate in the guest.
    /// This is done by taking several host and guest time readings, comparing
    /// elapsed time in the host to elapsed time in the guest, and declaring a
    /// "good" result if the percentage difference between them is within some
    /// tolerance. The check passes if the number of good results exceeds the
    /// number of bad results.
    async fn check_tsc(vm: &TestVm) -> anyhow::Result<()> {
        // The amount of error that can be tolerated in the guest's elapsed
        // time reading, expressed as a percentage of elapsed time on the host.
        //
        // If the reference TSC is working properly, host and guest time should
        // be very closely synchronized. However, because there is no way to
        // capture host and guest timestamps atomically, it will always appear
        // that more time has advanced in one domain than the other. The time
        // snapshotting logic tries to minimize this delta, but some error is
        // still expected, so a tolerance value is required.
        //
        // A 2.5% tolerance is *extremely* generous, but is necessary to keep
        // this test from flaking in CI runs. Generous as it is, this tolerance
        // value is still enough to catch egregious errors in computing TSC
        // scaling factors: shifting the scaling factor by the wrong number of
        // bits, for example, is liable to produce a much larger error than
        // this.
        const TOLERANCE: f64 = 0.025;

        // Take six readings to get five comparisons of consecutive readings.
        const NUM_READINGS: usize = 6;

        let mut readings = vec![];
        let mut good_diffs = 0;
        let mut bad_diffs = 0;

        for _ in 0..NUM_READINGS {
            readings.push(Reading::take_from(vm).await?);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

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

        assert!(
            bad_diffs < good_diffs,
            "more out-of-tolerance time diffs ({bad_diffs}) than in-tolerance \
            diffs ({good_diffs}); see test log for details",
        );

        info!(good_diffs, bad_diffs, "TSC test results");

        Ok(())
    }

    if ctx.default_guest_os_kind().await?.is_windows() {
        phd_skip!("test requires a guest with /proc/timer_list in procfs");
    }

    let mut cfg = ctx.vm_config_builder("hyperv_reference_tsc_elapsed_test");
    cfg.guest_hv_interface(GuestHypervisorInterface::HyperV {
        features: [HyperVFeatureFlag::ReferenceTsc].into_iter().collect(),
    });
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    check_tsc(&vm).await?;

    let mut target = ctx
        .spawn_successor_vm("hyperv_reference_tsc_elapsed_target", &vm, None)
        .await?;

    target
        .migrate_from(&vm, Uuid::new_v4(), MigrationTimeout::default())
        .await?;

    check_tsc(&vm).await?;
}
