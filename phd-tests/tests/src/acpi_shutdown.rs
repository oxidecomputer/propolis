// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

use phd_testcase::*;
use propolis_client::types::InstanceState;

#[phd_testcase]
async fn acpi_shutdown_stop(ctx: &TestCtx) {
    let mut vm = ctx.spawn_default_vm("acpi_shutdown_stop").await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    vm.acpi_shutdown(600).await?;
    // note: *before* the above elapsed
    // note note: so i guess alpine doesn't react to pwrbtn...
    // TODO: smell dmesg around when pwrbtn pressed?
    vm.wait_for_state(InstanceState::Stopped, Duration::from_secs(30)).await?;
}

#[phd_testcase]
async fn acpi_shutdown_reboot(ctx: &TestCtx) {
    let mut vm = ctx.spawn_default_vm("acpi_shutdown_reboot").await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    vm.acpi_reset(60).await?;
    vm.wait_for_state(InstanceState::Rebooting, Duration::from_secs(30))
        .await?;
}
