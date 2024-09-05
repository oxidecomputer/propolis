// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests that explicitly exercise the `instance_ensure` form of the VM start
//! API.

use phd_framework::{
    disk::BlockSize,
    test_vm::{DiskBackend, DiskInterface},
};
use phd_testcase::*;

#[phd_testcase]
async fn instance_ensure_api_test(ctx: &Framework) {
    if !ctx.crucible_enabled() {
        phd_skip!("test requires Crucible to be enabled");
    }

    let mut config = ctx.vm_config_builder("instance_ensure_api_test");
    config.boot_disk(
        ctx.default_guest_os_artifact(),
        DiskInterface::Nvme,
        DiskBackend::Crucible {
            min_disk_size_gib: 10,
            block_size: BlockSize::Bytes512,
        },
        0x10,
    );

    let mut vm = ctx.spawn_vm(&config, None).await?;
    vm.instance_ensure_using_api_request().await?;
}
