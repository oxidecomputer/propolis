// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_framework::{artifacts, lifecycle::Action, TestVm};
use phd_testcase::*;
use tracing::warn;

/// Attempts to see if the guest has detected Hyper-V support. This is
/// best-effort, since not all PHD guest images contain in-box tools that
/// display the current hypervisor vendor.
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
async fn hyperv_migration_smoke_test(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("hyperv_migration_smoke_test");
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
        &[Action::MigrateToPropolis(artifacts::DEFAULT_PROPOLIS_ARTIFACT)],
        |target: &TestVm| {
            Box::pin(async {
                guest_detect_hyperv(target).await.unwrap();
            })
        },
    )
    .await?;
}
