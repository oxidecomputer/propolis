// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_testcase::*;

#[phd_testcase]
async fn nproc_test(ctx: &Framework) {
    let mut vm =
        ctx.spawn_vm(ctx.vm_config_builder("nproc_test").cpus(6), None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let nproc = vm.run_shell_command("nproc").await?;
    assert_eq!(nproc.parse::<u8>().unwrap(), 6);
}

#[phd_testcase]
async fn api_reboot_test(ctx: &Framework) {
    let mut vm = ctx.spawn_default_vm("api_reboot_test").await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;
    vm.reset().await?;
    vm.wait_to_boot().await?;
}

#[phd_testcase]
async fn guest_reboot_test(ctx: &Framework) {
    let mut vm = ctx.spawn_default_vm("guest_reboot_test").await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    // Don't use `run_shell_command` because the guest won't echo another prompt
    // after this.
    vm.send_serial_str("reboot\n").await?;
    vm.wait_to_boot().await?;
}

#[phd_testcase]
async fn instance_spec_get_test(ctx: &Framework) {
    let mut vm = ctx
        .spawn_vm(
            ctx.vm_config_builder("instance_spec_test")
                .cpus(4)
                .memory_mib(3072),
            None,
        )
        .await?;
    vm.launch().await?;

    let spec_get_response = vm.get_spec().await?;
    let propolis_client::types::VersionedInstanceSpec::V0(spec) =
        spec_get_response.spec;
    assert_eq!(spec.devices.board.cpus, 4);
    assert_eq!(spec.devices.board.memory_mb, 3072);
}
