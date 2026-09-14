// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

use phd_framework::guest_os::GuestOsKind;
use phd_testcase::*;

use propolis_client::types::InstanceState;

/// start an alpine TestVm that will *not* respond to pwrbtn events.
async fn non_acpid_vm(
    ctx: &TestCtx,
    vm_name: &str,
) -> phd_testcase::Result<phd_framework::TestVm> {
    if ctx.default_guest_os_kind().await? != GuestOsKind::Alpine {
        // other more heavyweight distros probably listen for pwrbtn events
        phd_skip!("acpi_shutdown tests expect alpine default behaviors");
    }

    let mut vm = ctx.spawn_default_vm(vm_name).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;
    Ok(vm)
}

/// start an alpine TestVm that *will* respond to pwrbtn events.
async fn acpid_vm(
    ctx: &TestCtx,
    vm_name: &str,
) -> phd_testcase::Result<phd_framework::TestVm> {
    let vm = non_acpid_vm(ctx, vm_name).await?;

    // alpine won't respond to acpi pwrbtn events without acpid running.
    vm.run_shell_command("acpid").await?;

    Ok(vm)
}

#[phd_testcase]
async fn acpi_shutdown_stop(ctx: &TestCtx) {
    let vm = acpid_vm(ctx, "acpi_shutdown_stop").await?;

    // pwrbtn with intent to stop
    vm.acpi_shutdown(60).await?;

    // note: timeout *before* the above elapsed, we specifically want to see
    // that the guest reacted to the power button press by shutting itself down
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(30))
        .await?;
}

#[phd_testcase]
async fn acpi_shutdown_reboot(ctx: &TestCtx) {
    let vm = acpid_vm(ctx, "acpi_shutdown_reboot").await?;

    // pwrbtn with intent to reboot
    vm.acpi_reset(60).await?;

    // wait for the login prompt again
    vm.wait_to_boot().await?;
}

#[phd_testcase]
async fn acpi_shutdown_interject_with_hard_stop(ctx: &TestCtx) {
    let vm =
        non_acpid_vm(ctx, "acpi_shutdown_interject_with_hard_stop").await?;

    // note: *not* running acpid, so alpine will ignore pwrbtn.
    // sending a 'reset' request so we can be sure it was ignored when the
    // instance ends up destroyed
    vm.acpi_reset(60).await?;

    // give up waiting for timeout to elapse
    tokio::time::sleep(Duration::from_secs(3)).await;
    vm.stop().await?;

    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(30))
        .await?;
}

#[phd_testcase]
async fn acpi_shutdown_interject_with_hard_reboot(ctx: &TestCtx) {
    let vm =
        non_acpid_vm(ctx, "acpi_shutdown_interject_with_hard_reboot").await?;

    // note: *not* running acpid, so alpine will ignore pwrbtn.
    // sending a 'shutdown' request so we can be sure it was ignored when the
    // instance ends up at login prompt again
    vm.acpi_shutdown(60).await?;

    // give up waiting for timeout to elapse
    tokio::time::sleep(Duration::from_secs(3)).await;
    vm.reset().await?;

    vm.wait_to_boot().await?;
}

#[phd_testcase]
async fn acpi_shutdown_timeout_stop(ctx: &TestCtx) {
    let vm = non_acpid_vm(ctx, "acpi_shutdown_timeout_stop").await?;

    // note: *not* running acpid, so alpine should ignore pwrbtn
    // and be hard-stopped in 1 second
    vm.acpi_shutdown(1).await?;

    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(5)).await?;
}

#[phd_testcase]
async fn acpi_shutdown_timeout_reboot(ctx: &TestCtx) {
    let vm = non_acpid_vm(ctx, "acpi_shutdown_timeout_reboot").await?;

    // note: *not* running acpid, so alpine should ignore pwrbtn
    // and be hard-reset in 1 second
    vm.acpi_reset(1).await?;

    // wait for the login prompt again
    vm.wait_to_boot().await?;
}
