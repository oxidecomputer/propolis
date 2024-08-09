// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_framework::{
    disk::{fat::FatFilesystem, DiskSource},
    test_vm::{DiskBackend, DiskInterface},
};
use phd_testcase::*;
use tracing::{info, warn};

#[phd_testcase]
async fn in_memory_backend_smoke_test(ctx: &Framework) {
    const HELLO_MSG: &str = "hello oxide!";

    let mut cfg = ctx.vm_config_builder("in_memory_backend_test");
    let mut data = FatFilesystem::new();
    data.add_file_from_str("hello_oxide.txt", HELLO_MSG)?;
    cfg.data_disk(
        DiskSource::FatFilesystem(data),
        DiskInterface::Virtio,
        DiskBackend::InMemory { readonly: true },
        24,
    );

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    // Some guests expose a /dev/disk/by-path directory that contains symlinks
    // mapping PCI paths to the underlying device nodes under /dev. If this is
    // present, check the mapping for a virtio disk at 0.24.0.
    //
    // The commands after this try to mount the in-memory disk and assume that
    // its device is located at /dev/vda. If the by-path directory is present,
    // try to check that the disk is located there and fail the test early if
    // it's not. If the by-path directory is missing, put up a warning and hope
    // for the best.
    let dev_disk = vm.run_shell_command("ls /dev/disk").await?;
    if dev_disk.contains("by-path") {
        let ls = vm.run_shell_command("ls -la /dev/disk/by-path").await?;
        info!(%ls, "guest disk device paths");
        assert!(ls.contains("virtio-pci-0000:00:18.0 -> ../../vda"));
    } else {
        warn!(
            "guest doesn't support /dev/disk/by-path, did not verify device \
            path"
        );
    }

    vm.run_shell_command("mkdir /phd").await?;

    // The disk is read-only, so pass the `ro` option to `mount` so that it
    // doesn't complain about not being able to mount for writing.
    let mount = vm.run_shell_command("mount -o ro /dev/vda /phd").await?;
    assert_eq!(mount, "");

    // The file should be there and have the expected contents.
    let ls = vm.run_shell_command("ls /phd").await?;
    assert_eq!(ls, "hello_oxide.txt");

    let cat = vm.run_shell_command("cat /phd/hello_oxide.txt").await?;
    assert_eq!(cat, HELLO_MSG);
}
