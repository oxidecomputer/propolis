// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

use phd_framework::{
    disk::{BlockSize, DiskSource},
    test_vm::{DiskBackend, DiskInterface},
};
use phd_testcase::*;
use propolis_client::types::InstanceState;

#[phd_testcase]
async fn boot_test(ctx: &Framework) {
    let mut config = ctx.vm_config_builder("crucible_boot_test");
    super::add_default_boot_disk(ctx, &mut config)?;
    let mut vm = ctx.spawn_vm(&config, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;
}

#[phd_testcase]
async fn api_reboot_test(ctx: &Framework) {
    let mut config = ctx.vm_config_builder("crucible_guest_reboot_test");
    super::add_default_boot_disk(ctx, &mut config)?;

    let mut vm = ctx.spawn_vm(&config, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;
    vm.reset().await?;
    vm.wait_to_boot().await?;
}

#[phd_testcase]
async fn guest_reboot_test(ctx: &Framework) {
    let mut config = ctx.vm_config_builder("crucible_guest_reboot_test");
    super::add_default_boot_disk(ctx, &mut config)?;

    let mut vm = ctx.spawn_vm(&config, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    vm.graceful_reboot().await?;
}

#[phd_testcase]
async fn shutdown_persistence_test(ctx: &Framework) {
    let mut config =
        ctx.vm_config_builder("crucible_shutdown_persistence_test");
    super::add_default_boot_disk(ctx, &mut config)?;
    let mut vm = ctx.spawn_vm(&config, None).await?;
    if vm.guest_os_has_read_only_fs() {
        phd_skip!(
            "Can't run data persistence test on a guest with a read-only file
             system"
        );
    }

    let disk_handles = vm.cloned_disk_handles();
    let disk = disk_handles[0].as_crucible().unwrap();
    disk.set_generation(1);
    vm.launch().await?;
    vm.wait_to_boot().await?;

    // Verify that the test file doesn't exist yet, then touch it, flush it, and
    // shut down the VM.
    let lsout = vm.run_shell_command("ls foo.bar 2> /dev/null").await?;
    assert_eq!(lsout, "");
    vm.run_shell_command("touch ./foo.bar").await?;
    vm.run_shell_command("sync ./foo.bar").await?;
    vm.stop().await?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))
        .await?;

    // Increment the disk's generation before attaching it to a new VM.
    disk.set_generation(2);
    let mut vm = ctx
        .spawn_successor_vm("crucible_shutdown_persistence_test_2", &vm, None)
        .await?;

    vm.launch().await?;
    vm.wait_to_boot().await?;

    // The touched file from the previous VM should be present in the new one.
    let lsout = vm.run_shell_command("ls foo.bar 2> /dev/null").await?;
    assert_eq!(lsout, "foo.bar");
}

#[phd_testcase]
async fn vcr_replace_during_start_test(ctx: &Framework) {
    let mut config =
        ctx.vm_config_builder("crucible_vcr_replace_during_start_test");

    // Create a blank data disk on which to perform VCR replacement. This is
    // necessary because Crucible doesn't permit VCR replacements for volumes
    // whose read-only parents are local files (which is true for artifact-based
    // Crucible disks).
    const DATA_DISK_NAME: &str = "vcr-replacement-target";
    config.data_disk(
        DATA_DISK_NAME,
        DiskSource::Blank(1024 * 1024 * 1024),
        DiskInterface::Nvme,
        DiskBackend::Crucible {
            min_disk_size_gib: 1,
            block_size: BlockSize::Bytes512,
        },
        5,
    );

    // Configure the disk so that when the VM starts, it will have an invalid
    // downstairs address.
    let spec = config.vm_spec(ctx).await?;
    let disk_hdl =
        spec.get_disk_by_device_name(DATA_DISK_NAME).cloned().unwrap();
    let disk = disk_hdl.as_crucible().unwrap();
    disk.enable_vcr_black_hole();

    // Try to start the VM, but don't wait for it to boot; it should get stuck
    // while activating using an invalid downstairs address.
    let mut vm = ctx.spawn_vm_with_spec(spec, None).await?;
    vm.launch().await?;

    // The VM is expected not to reach the Running state. Unfortunately, there's
    // no great way to test that this is never going to happen; as a best-effort
    // alternative, wait for a short while and assert that the VM doesn't reach
    // Running in the timeout interval.
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(5))
        .await
        .unwrap_err();

    // Fix the disk's downstairs address and send a replacement request. This
    // should be processed and should allow the VM to boot.
    disk.disable_vcr_black_hole();
    disk.set_generation(2);
    vm.replace_crucible_vcr(disk).await?;
    vm.wait_to_boot().await?;

    assert_eq!(vm.get().await?.instance.state, InstanceState::Running);

    // VCR replacements should continue to be accepted now that the instance is
    // running.
    disk.set_generation(3);
    vm.replace_crucible_vcr(disk).await?;
}
