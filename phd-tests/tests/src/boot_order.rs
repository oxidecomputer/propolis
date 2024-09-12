// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::{bail, Error};
use phd_framework::{
    disk::{fat::FatFilesystem, DiskSource},
    test_vm::{DiskBackend, DiskInterface},
};
use phd_testcase::*;
use std::io::Cursor;
use tracing::warn;

mod efi_utils;

use efi_utils::{
    bootvar, discover_boot_option_numbers, efipath, find_option_in_boot_order,
    read_efivar, remove_boot_entry, write_efivar, EfiLoadOption,
    BOOT_CURRENT_VAR, BOOT_ORDER_VAR, EDK2_EFI_SHELL_GUID,
    EDK2_FIRMWARE_VOL_GUID, EDK2_UI_APP_GUID,
};

pub(crate) async fn run_long_command(
    vm: &phd_framework::TestVm,
    cmd: &str,
) -> Result<String, Error> {
    // Ok, this is a bit whacky: something about the line wrapping for long commands causes
    // `run_shell_command` to hang instead of ever actually seeing a response prompt.
    //
    // I haven't gone and debugged that; instead, chunk the input command up into segments short
    // enough to not wrap when input, put them all in a file, then run the file.
    vm.run_shell_command("rm cmd").await?;
    let mut offset = 0;
    // Escape any internal `\`. This isn't comprehensive escaping (doesn't handle \n, for
    // example)..
    let cmd = cmd.replace("\\", "\\\\");
    while offset < cmd.len() {
        let lim = std::cmp::min(cmd.len() - offset, 50);
        let chunk = &cmd[offset..][..lim];
        offset += lim;

        // Catch this before it causes weird issues in half-executed commands.
        //
        // Could escape these here, but right now that's not really necessary.
        assert!(!chunk.contains("\n"));

        vm.run_shell_command(&format!("echo -n \'{}\' >>cmd", chunk)).await?;
    }
    vm.run_shell_command(". cmd").await
}

// This test checks that with a specified boot order, the guest boots whichever disk we wanted to
// come first. This is simple enough, until you want to know "what you booted from"..
//
// For live CDs, such as Alpine's, the system boots into a tmpfs loaded from a boot disk, but
// there's no clear line to what disk that live image *came from*. If you had two Alpine 3.20.3
// images attached to one VM, you'd ceretainly boot into Alpine 3.20.3, but I don't see a way to
// tell *which disk* that Alpine would be sourced from, from Alpine alone.
//
// So instead, check EFI variables. To do this, then, we have to.. parse EFI variables. That is
// what this test does below, but it's probably not fully robust to what we might do with PCI
// devices in the future.
//
// A more "future-proof" setup might be to just boot an OS, see that we ended up in the OS we
// expected, and check some attribute about it like that the kernel version is what we expected the
// booted OS to be. That's still a good fallback if we discover that parsing EFI variables is
// difficult to stick with for any reason. It has a downside though: we'd have to keep a specific
// image around with a specific kernel version as either the "we expect to boot into this" image
// or the "we expected to boot into not this" cases.
//
// The simplest case: show that we can configure the guest's boot order from outside the machine.
// This is the most likely common case, where Propolis is told what the boot order should be by
// Nexus and we simply make it happen.
//
// Unlike later tests, this test does not manipulate boot configuration from inside the guest OS.
#[phd_testcase]
async fn configurable_boot_order(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("configurable_boot_order");
    cfg.boot_disk(
        ctx.default_guest_os_artifact(),
        DiskInterface::Virtio,
        DiskBackend::File,
        4,
    );

    // Create a second disk backed by the same guest OS. This way we'll boot to the same
    // environment regardless of which disk is used; we'll check EFI variables to figure out if the
    // right disk was booted.
    cfg.data_disk(
        "alt-boot",
        DiskSource::Artifact(ctx.default_guest_os_artifact()),
        DiskInterface::Virtio,
        DiskBackend::File,
        24,
    );

    // We haven't specified a boot order. So, we'll expect that we boot to the lower-numbered PCI
    // device (4) and end up in Alpine 3.20.
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let boot_num_bytes = read_efivar(&vm, BOOT_CURRENT_VAR).await?;

    let boot_num: u16 = u16::from_le_bytes(boot_num_bytes.try_into().unwrap());

    let boot_option_bytes = read_efivar(&vm, &bootvar(boot_num)).await?;

    let mut cursor = Cursor::new(boot_option_bytes.as_slice());

    let load_option = EfiLoadOption::parse_from(&mut cursor).unwrap();

    assert!(load_option.path.matches_pci_device_function(4, 0));

    // Now specify a boot order and do the whole thing again. Note that this order puts the later
    // PCI device first, so this changes the boot order!
    cfg.boot_order(vec!["alt-boot", "boot-disk"]);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let boot_num_bytes = read_efivar(&vm, BOOT_CURRENT_VAR).await?;

    let boot_num: u16 = u16::from_le_bytes(boot_num_bytes.try_into().unwrap());

    let boot_option_bytes = read_efivar(&vm, &bootvar(boot_num)).await?;

    let mut cursor = Cursor::new(boot_option_bytes.as_slice());

    let load_option = EfiLoadOption::parse_from(&mut cursor).unwrap();

    // If we were going to test the PCI bus number too, we'd check the AHCI Device Path entry that
    // precedes these PCI values. But we only use PCI bus 0 today, and the mapping from an AHCI
    // Device Path to a PCI root is not immediately obvious?
    assert!(load_option.path.matches_pci_device_function(24, 0));
}

// This is very similar to the `in_memory_backend_smoke_test` test, but specifically asserts that
// the unbootable disk is first in the boot order; the system booting means that boot order is
// respected and a non-bootable disk does not wedge startup.
#[phd_testcase]
async fn unbootable_disk_skipped(ctx: &Framework) {
    const HELLO_MSG: &str = "hello oxide!";

    let mut cfg = ctx.vm_config_builder("unbootable_disk_skipped");
    let mut unbootable = FatFilesystem::new();
    unbootable.add_file_from_str("hello_oxide.txt", HELLO_MSG)?;
    cfg.data_disk(
        "unbootable",
        DiskSource::FatFilesystem(unbootable),
        DiskInterface::Virtio,
        DiskBackend::InMemory { readonly: true },
        16,
    );

    // `boot-disk` is the implicitly-created boot disk made from the default guest OS artifact.
    //
    // explicitly boot from it later, so OVMF has to try and fail to boot `unbootable`.
    cfg.boot_order(vec!["unbootable", "boot-disk"]);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let boot_num_bytes = read_efivar(&vm, BOOT_CURRENT_VAR).await?;

    let boot_num: u16 = u16::from_le_bytes(boot_num_bytes.try_into().unwrap());

    let boot_option_bytes = read_efivar(&vm, &bootvar(boot_num)).await?;

    let mut cursor = Cursor::new(boot_option_bytes.as_slice());

    let load_option = EfiLoadOption::parse_from(&mut cursor).unwrap();

    // Device 4 is the implicitly-used `boot-disk` PCI device number. This is not 16, for example,
    // as we expect to not boot `unbootable`.
    assert_eq!(load_option.pci_device_function(), (4, 0));

    let boot_order_bytes = read_efivar(&vm, BOOT_ORDER_VAR).await?;

    // Interestingly, when we specify a boot order via fwcfg, OVMF includes two additional entries:
    // * "UiApp", which I can't find much about
    // * "EFI Internal Shell", the EFI shell the system drops into if no disks are bootable
    //
    // Exactly where these end up in the boot order is not entirely important; we really just need
    // to make sure that the boot order we specified comes first (and before "EFI Internal Shell")
    #[derive(Debug, PartialEq, Eq)]
    enum TestState {
        SeekingUnbootable,
        FoundUnbootable,
        AfterBootOrder,
    }

    let mut state = TestState::SeekingUnbootable;

    for item in boot_order_bytes.chunks(2) {
        let option_num: u16 = u16::from_le_bytes(item.try_into().unwrap());

        let option_bytes = read_efivar(&vm, &bootvar(option_num)).await?;

        let mut cursor = Cursor::new(option_bytes.as_slice());

        let load_option = EfiLoadOption::parse_from(&mut cursor).unwrap();

        match state {
            TestState::SeekingUnbootable => {
                if load_option.path.matches_pci_device_function(16, 0) {
                    state = TestState::FoundUnbootable;
                    continue;
                } else if load_option
                    .path
                    .matches_fw_file(EDK2_FIRMWARE_VOL_GUID, EDK2_UI_APP_GUID)
                {
                    // `UiApp`. Ignore it and continue.
                    continue;
                } else {
                    bail!(
                        "Did not expect to find {:?} yet (test state = {:?})",
                        load_option,
                        state
                    );
                }
            }
            TestState::FoundUnbootable => {
                if load_option.path.matches_pci_device_function(4, 0) {
                    state = TestState::AfterBootOrder;
                    continue;
                } else {
                    bail!(
                        "Did not expect to find {:?} (test state = {:?})",
                        load_option,
                        state
                    );
                }
            }
            TestState::AfterBootOrder => {
                let is_ui_app = load_option
                    .path
                    .matches_fw_file(EDK2_FIRMWARE_VOL_GUID, EDK2_UI_APP_GUID);
                let is_efi_shell = load_option.path.matches_fw_file(
                    EDK2_FIRMWARE_VOL_GUID,
                    EDK2_EFI_SHELL_GUID,
                );
                if !is_ui_app && !is_efi_shell {
                    bail!(
                        "Did not expect to find {:?} (test state = {:?})",
                        load_option,
                        state
                    );
                }
            }
        }
    }

    assert_eq!(state, TestState::AfterBootOrder);
}

// Start with the boot order being `["boot-disk", "unbootable"]`, then change it so that next boot
// we'll boot from `unbootable` first. Then reboot and verify that the boot order is still
// "boot-disk" first.
//
// TODO(ixi): This would be a bit nicer if there was a secondary image to confirm we've booted into
// instead...
#[phd_testcase]
async fn guest_can_adjust_boot_order(ctx: &Framework) {
    const HELLO_MSG: &str = "hello oxide!";

    let mut cfg = ctx.vm_config_builder("guest_can_adjust_boot_order");
    let mut unbootable = FatFilesystem::new();
    unbootable.add_file_from_str("hello_oxide.txt", HELLO_MSG)?;
    cfg.boot_disk("ubuntu-noble", DiskInterface::Virtio, DiskBackend::File, 4);

    cfg.data_disk(
        "unbootable",
        DiskSource::FatFilesystem(unbootable),
        DiskInterface::Virtio,
        DiskBackend::InMemory { readonly: true },
        16,
    );

    // `boot-disk` is the implicitly-created boot disk made from the default guest OS artifact.
    //
    // explicitly boot from it later, so OVMF has to try and fail to boot `unbootable`.
    cfg.boot_order(vec!["boot-disk"]); //, "unbootable"]);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    // If the guest doesn't have an EFI partition then there's no way for boot order preferences to
    // be persisted.
    let mountline = vm.run_shell_command("mount | grep /boot/efi").await?;

    if !mountline.contains(" on /boot/efi type vfat") {
        warn!("guest doesn't have an EFI partition, cannot manage boot order");
    }

    // Try adding a few new boot options, then add them to the boot order, reboot, and make sure
    // they're all as we set them.
    if !run_long_command(&vm, &format!("ls {}", efipath(&bootvar(0xffff))))
        .await?
        .is_empty()
    {
        warn!("guest environment already has a BootFFFF entry; is this not a fresh image?");
    }

    let boot_num: u16 = {
        let bytes = read_efivar(&vm, BOOT_CURRENT_VAR).await?;
        u16::from_le_bytes(bytes.try_into().unwrap())
    };

    // The entry we booted from is clearly valid, so we should be able to insert a few duplicate
    // entries. We won't boot into them, but if something bogus happens and we did boot one of
    // these, at least it'll work and we can detect the misbehavior.
    //
    // But here's a weird one: if we just append these to the end, on reboot they'll be moved
    // somewhat up the boot order. This occurrs both if setting variables through `efibootmgr` or
    // by writing to /sys/firmware/efi/efivars/BootOrder-* directly. As an example, say we had a
    // boot order of "0004,0001,0003,0000" where boot options were as follows:
    // * 0000: UiApp
    // * 0001: PCI device 4, function 0
    // * 0003: EFI shell (Firmware volume+file)
    // * 0004: Ubuntu (HD partition 15, GPT formatted)
    //
    // If we duplicate entry 4 to new options FFF0 and FFFF, reset the boot order to
    // "0004,0001,0003,0000,FFF0,FFFF", then reboot the VM, the boot order when it comes back
    // up will be "0004,0001,FFF0,FFFF,0003,0000".
    //
    // This almost makes sense, but with other devices in the mix I've seen reorderings like
    // `0004,0001,<PCI 16.0>,0003,0000,FFF0,FFFF` turning into
    // `0004,0001,FFF0,FFFF,<PCI 16.0>,0003,0000`. This is particularly strange in that the new
    // options were reordered around some other PCI device. It's not the boot order we set!
    //
    // So, to at least confirm we *can* modify the boot order in a stable way, make a somewhat less
    // ambitious change: insert the duplicate boot options in the order directly after the option
    // they are duplicates of. This seems to not get reordered.
    let boot_option_bytes = read_efivar(&vm, &bootvar(boot_num)).await?;

    let boot_order_bytes = read_efivar(&vm, BOOT_ORDER_VAR).await?;

    let mut new_boot_order = Vec::new();
    new_boot_order.extend_from_slice(&boot_order_bytes);

    let mut new_boot_order = boot_order_bytes.clone();
    let booted_idx = new_boot_order
        .chunks(2)
        .enumerate()
        .find(|(_i, chunk)| *chunk == boot_num.to_le_bytes())
        .map(|(i, _chunk)| i)
        .expect("booted entry exists");
    let suffix = new_boot_order.split_off((booted_idx + 1) * 2);
    new_boot_order.extend_from_slice(&[0xf0, 0xff]);
    new_boot_order.extend_from_slice(&[0xff, 0xff]);
    new_boot_order.extend_from_slice(&suffix);

    write_efivar(&vm, &bootvar(0xfff0), &boot_option_bytes).await?;
    assert_eq!(boot_option_bytes, read_efivar(&vm, &bootvar(0xfff0)).await?);
    write_efivar(&vm, &bootvar(0xffff), &boot_option_bytes).await?;
    assert_eq!(boot_option_bytes, read_efivar(&vm, &bootvar(0xffff)).await?);

    write_efivar(&vm, BOOT_ORDER_VAR, &new_boot_order).await?;
    let written_boot_order = read_efivar(&vm, BOOT_ORDER_VAR).await?;
    assert_eq!(new_boot_order, written_boot_order);

    // Now, reboot and check that the settings stuck.
    vm.run_shell_command("reboot").await?;
    vm.wait_to_boot().await?;

    let boot_order_after_reboot = read_efivar(&vm, BOOT_ORDER_VAR).await?;
    assert_eq!(new_boot_order, boot_order_after_reboot);

    let boot_num_after_reboot: u16 = {
        let bytes = read_efivar(&vm, BOOT_CURRENT_VAR).await?;
        u16::from_le_bytes(bytes.try_into().unwrap())
    };
    assert_eq!(boot_num, boot_num_after_reboot);

    let boot_option_bytes_after_reboot =
        read_efivar(&vm, &bootvar(boot_num)).await?;
    assert_eq!(boot_option_bytes, boot_option_bytes_after_reboot);
}

// and then one more test about what happens if configure the boot order in the guest, stop the VM,
// change the configured boot order, and start the VM again...
