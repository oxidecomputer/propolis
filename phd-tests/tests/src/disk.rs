// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_framework::{
    disk::{fat::FatFilesystem, DiskSource},
    guest_os::GuestOsKind,
    test_vm::{DiskBackend, DiskInterface, MigrationTimeout},
    TestVm,
};
use phd_testcase::*;
use uuid::Uuid;

/// Creates a VM with an in-memory disk backed by the supplied `data`, waits for
/// it to boot, and issues some shell commands to find the
///
/// Returns a tuple containing the created VM and the path to the guest disk
/// device representing the in-memory disk.
async fn launch_vm_and_find_in_memory_disk(
    ctx: &Framework,
    vm_name: &str,
    data: DiskSource<'_>,
    readonly: bool,
) -> anyhow::Result<(TestVm, String)> {
    let mut cfg = ctx.vm_config_builder(vm_name);
    cfg.data_disk(
        "data-disk-0",
        data,
        DiskInterface::Virtio,
        DiskBackend::InMemory { readonly },
        24,
    );
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let device_path = if let Some(vm) = vm.get_windows_vm() {
        // Cygwin documents that \Device\HardDisk devices in the NT device
        // namespace map to /dev/sd devices in the emulated POSIX namespace:
        // disk 0 is /dev/sda, disk 1 is /dev/sdb, and so on. Get the NT device
        // number of the attached in-memory disk.
        let cmd = "(Get-PhysicalDisk | Where {$_.BusType -ne 'NVMe'}).DeviceId";
        let num = vm.run_powershell_command(cmd).await?.parse::<u8>()?;

        // If the test requires the disk to be writable, run diskpart to ensure
        // that its readonly attribute is cleared.
        if !readonly {
            vm.run_shell_command(&format!(
                "echo 'select disk {num}' >> diskpart.txt"
            ))
            .await?;
            vm.run_shell_command(
                "echo 'attributes disk clear readonly' >> diskpart.txt",
            )
            .await?;
            vm.run_shell_command("diskpart /s diskpart.txt").await?;
        }

        // Crudely map from the drive number to the appropriate letter suffix.
        // Cygwin supports more than 26 drives (up to /dev/sddx), but the data
        // disk shouldn't map into that range unless Windows does something
        // unexpected with its drive number assignments.
        assert!(
            num < 26,
            "physical drive number must be less than 26 to map to a Cygwin dev"
        );

        format!("/dev/sd{}", (b'a' + num) as char)
    } else {
        let ls = vm
            .run_shell_command(
                "ls /sys/devices/pci0000:00/0000:00:18.0/virtio0/block",
            )
            .await?;

        format!("/dev/{ls}")
    };

    Ok((vm, device_path))
}

async fn mount_in_memory_disk(
    vm: &mut TestVm,
    device_path: &str,
    readonly: bool,
) -> anyhow::Result<()> {
    if vm.guest_os_kind().is_windows() {
        phd_skip!(
            "in-memory disk tests use mount options not supported by Cygwin"
        );
    }

    vm.run_shell_command("mkdir /phd").await?;

    // If the disk is read-only, add the `ro` qualifier to the mount command
    // so that it doesn't complain about being unable to mount for writing.
    if readonly {
        let mount = vm
            .run_shell_command(&format!("mount -o ro {device_path} /phd"))
            .await?;
        assert_eq!(mount, "");
    } else {
        vm.run_shell_command(&format!(
            "echo '{device_path} /phd vfat defaults 0 2' >> /etc/fstab"
        ))
        .await?;

        let mount = vm.run_shell_command("mount -a").await?;
        assert_eq!(mount, "");
    }

    Ok(())
}

#[phd_testcase]
async fn in_memory_backend_smoke_test(ctx: &Framework) {
    if ctx.default_guest_os_kind().await?.is_windows() {
        phd_skip!(
            "in-memory disk tests use mount options not supported by Cygwin"
        );
    }

    const HELLO_MSG: &str = "hello oxide!";

    let readonly = true;
    let mut data = FatFilesystem::new();
    data.add_file_from_str("hello_oxide.txt", HELLO_MSG)?;
    let (mut vm, device_path) = launch_vm_and_find_in_memory_disk(
        ctx,
        "in_memory_backend_test",
        DiskSource::FatFilesystem(data),
        readonly,
    )
    .await?;

    mount_in_memory_disk(&mut vm, &device_path, readonly).await?;

    // The file should be there and have the expected contents.
    let ls = vm.run_shell_command("ls /phd").await?;
    assert_eq!(ls, "hello_oxide.txt");

    let cat = vm.run_shell_command("cat /phd/hello_oxide.txt").await?;
    assert_eq!(cat, HELLO_MSG);
}

#[phd_testcase]
async fn in_memory_backend_migration_test(ctx: &Framework) {
    // A blank disk is fine for this test: the rest of the test will address the
    // disk device directly instead of assuming it has a file system. This works
    // around #824 for Windows guests (which may not recognize the FAT
    // filesystems PHD produces).
    let (vm, device_path) = launch_vm_and_find_in_memory_disk(
        ctx,
        "in_memory_backend_migration_test_source",
        DiskSource::Blank(16 * 1024),
        false,
    )
    .await?;

    // Scribble random data into the first kilobyte of the data disk, passing
    // the appropriate flags to ensure that the guest actually writes the data
    // to the disk (instead of just holding it in memory).
    let force_sync = if let GuestOsKind::Alpine = vm.guest_os_kind() {
        "conv=sync"
    } else {
        "oflag=sync"
    };

    vm.run_shell_command(&format!(
        "dd if=/dev/random of={device_path} {force_sync} bs=1K count=1"
    ))
    .await?;

    // Read the scribbled data out to a file on the main OS disk.
    vm.run_shell_command(&format!(
        "dd if={device_path} of=/tmp/before iflag=direct bs=1K"
    ))
    .await?;

    // Migrate the VM.
    let mut target = ctx
        .spawn_successor_vm(
            "in_memory_backend_migration_test_target",
            &vm,
            None,
        )
        .await?;

    target
        .migrate_from(&vm, Uuid::new_v4(), MigrationTimeout::default())
        .await?;

    // Read the scribbled data back from the disk. On most guests, adding
    // `iflag=direct` to the `dd` invocation is sufficient to bypass the guest's
    // caches and read from the underlying disk. Alpine guests appear also to
    // need a procfs poke to drop page caches before they'll read from the disk.
    if let GuestOsKind::Alpine = vm.guest_os_kind() {
        target.run_shell_command("sync").await?;
        target.run_shell_command("echo 3 > /proc/sys/vm/drop_caches").await?;
    }

    target
        .run_shell_command(&format!(
            "dd if={device_path} of=/tmp/after iflag=direct bs=1K"
        ))
        .await?;

    // The data that was scribbled before migrating should match what was read
    // back from the disk. If it doesn't, migration restored the original
    // (blank) disk contents, which is incorrect.
    let out = target
        .run_shell_command("diff --report-identical /tmp/before /tmp/after")
        .await?;

    assert_eq!(out, "Files /tmp/before and /tmp/after are identical");
}
