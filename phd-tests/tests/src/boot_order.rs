// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_framework::{
    disk::DiskSource,
    test_vm::{DiskBackend, DiskInterface},
};
use phd_testcase::*;
// use tracing::{info, warn};

#[phd_testcase]
async fn configurable_boot_order(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("configurable_boot_order");
    cfg.data_disk(
        "alpine-3-20",
        DiskSource::Artifact("alpine-3-20"),
        DiskInterface::Virtio,
        DiskBackend::File,
        16,
    );

    cfg.data_disk(
        "alpine-3-16",
        DiskSource::Artifact("alpine-3-16"),
        DiskInterface::Virtio,
        DiskBackend::File,
        24,
    );

    cfg.boot_order(vec!["alpine-3-20", "alpine-3-16"]);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let kernel = vm.run_shell_command("uname -r").await?;
    if kernel.contains("6.6.41-0-virt") {
        panic!("booted 6.6.41");
    } else if kernel.contains("5.15.41-0-virt") {
        panic!("booted 5.15.41");
    } else {
        eprintln!("dunno what we booted? {}", kernel);
    }
}
