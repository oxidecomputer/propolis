// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

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
