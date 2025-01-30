// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_testcase::*;
use tracing::info;

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

    let out = vm.run_shell_command("systemd-detect-virt").await?;
    info!(out, "detected hypervisor");
}
