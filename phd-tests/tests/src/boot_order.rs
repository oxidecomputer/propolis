// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::bail;
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

// This test checks that with a specified boot order, the guest boots whichever
// disk we wanted to come first. This is simple enough, until you want to know
// "what you booted from"..
//
// For live CDs, such as Alpine's, the system boots into a tmpfs loaded from a
// boot disk, but there's no clear line to what disk that live image *came
// from*. If you had two Alpine 3.20.3 images attached to one VM, you'd
// ceretainly boot into Alpine 3.20.3, but I don't see a way to tell *which
// disk* that Alpine would be sourced from, from Alpine alone.
//
// So instead, check EFI variables. To do this, then, we have to.. parse EFI
// variables. That is what this test does below, but it's probably not fully
// robust to what we might do with PCI devices in the future.
//
// A more "future-proof" setup might be to just boot an OS, see that we ended up
// in the OS we expected, and check some attribute about it like that the kernel
// version is what we expected the booted OS to be. That's still a good fallback
// if we discover that parsing EFI variables is difficult to stick with for any
// reason. It has a downside though: we'd have to keep a specific image around
// with a specific kernel version as either the "we expect to boot into this"
// image or the "we expected to boot into not this" cases.
//
// The simplest case: show that we can configure the guest's boot order from
// outside the machine.  This is the most likely common case, where Propolis is
// told what the boot order should be by Nexus and we simply make it happen.
//
// Unlike later tests, this test does not manipulate boot configuration from
// inside the guest OS.
#[phd_testcase]
async fn configurable_boot_order(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("configurable_boot_order");

    // Create a second disk backed by the same artifact as the default
    // `boot-disk`. This way we'll boot to the same environment regardless of
    // which disk is used; we'll check EFI variables to figure out if the right
    // disk was booted.
    cfg.data_disk(
        "alt-boot",
        DiskSource::Artifact(ctx.default_guest_os_artifact()),
        DiskInterface::Virtio,
        DiskBackend::File,
        24,
    );

    // We haven't specified a boot order. So, we'll expect that we boot to the
    // lower-numbered PCI device (4) and end up in Alpine 3.20.
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    if !vm.guest_os_kind().is_linux() {
        phd_skip!("boot order tests require efivarfs to manipulate UEFI vars");
    }
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let boot_num_bytes = read_efivar(&vm, BOOT_CURRENT_VAR).await?;

    let boot_num: u16 = u16::from_le_bytes(boot_num_bytes.try_into().unwrap());

    let boot_option_bytes = read_efivar(&vm, &bootvar(boot_num)).await?;

    let mut cursor = Cursor::new(boot_option_bytes.as_slice());

    let load_option = EfiLoadOption::parse_from(&mut cursor).unwrap();

    assert!(load_option.path.matches_pci_device_function(4, 0));

    // Now specify a boot order and do the whole thing again. Note that this
    // order puts the later PCI device first, so this changes the boot order!
    cfg.boot_order(vec!["alt-boot", "boot-disk"]);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let boot_num_bytes = read_efivar(&vm, BOOT_CURRENT_VAR).await?;

    let boot_num: u16 = u16::from_le_bytes(boot_num_bytes.try_into().unwrap());

    let boot_option_bytes = read_efivar(&vm, &bootvar(boot_num)).await?;

    let mut cursor = Cursor::new(boot_option_bytes.as_slice());

    let load_option = EfiLoadOption::parse_from(&mut cursor).unwrap();

    // If we were going to test the PCI bus number too, we'd check the AHCI
    // Device Path entry that precedes these PCI values. But we only use PCI bus
    // 0 today, and the mapping from an AHCI Device Path to a PCI root is not
    // immediately obvious?
    assert!(load_option.path.matches_pci_device_function(24, 0));
}

// This is very similar to the `in_memory_backend_smoke_test` test, but
// specifically asserts that the unbootable disk is first in the boot order; the
// system booting means that boot order is respected and a non-bootable disk
// does not wedge startup.
#[phd_testcase]
async fn unbootable_disk_skipped(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("unbootable_disk_skipped");

    cfg.data_disk(
        "unbootable",
        DiskSource::FatFilesystem(FatFilesystem::new()),
        DiskInterface::Virtio,
        DiskBackend::InMemory { readonly: true },
        16,
    );

    // `boot-disk` is the implicitly-created boot disk made from the default
    // guest OS artifact.
    //
    // explicitly boot from it later, so OVMF has to try and fail to boot
    // `unbootable`.
    cfg.boot_order(vec!["unbootable", "boot-disk"]);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    if !vm.guest_os_kind().is_linux() {
        phd_skip!("boot order tests require efivarfs to manipulate UEFI vars");
    }
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let boot_num_bytes = read_efivar(&vm, BOOT_CURRENT_VAR).await?;

    let boot_num: u16 = u16::from_le_bytes(boot_num_bytes.try_into().unwrap());

    let boot_option_bytes = read_efivar(&vm, &bootvar(boot_num)).await?;

    let mut cursor = Cursor::new(boot_option_bytes.as_slice());

    let load_option = EfiLoadOption::parse_from(&mut cursor).unwrap();

    // Device 4 is the implicitly-used `boot-disk` PCI device number. This is
    // not 16, for example, as we expect to not boot `unbootable`.
    assert_eq!(load_option.pci_device_function(), (4, 0));

    let boot_order_bytes = read_efivar(&vm, BOOT_ORDER_VAR).await?;

    // Interestingly, when we specify a boot order via fwcfg, OVMF includes two
    // additional entries:
    // * "UiApp", which I can't find much about
    // * "EFI Internal Shell", the EFI shell the system drops into if no disks
    //   are bootable
    //
    // Exactly where these end up in the boot order is not entirely important;
    // we really just need to make sure that the boot order we specified comes
    // first (and before "EFI Internal Shell")
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

// Start with the boot order being `["boot-disk", "unbootable"]`, then change it
// so that next boot we'll boot from `unbootable` first. Then reboot and verify
// that the boot order is still "boot-disk" first.
#[phd_testcase]
async fn guest_can_adjust_boot_order(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("guest_can_adjust_boot_order");

    cfg.data_disk(
        "unbootable",
        DiskSource::FatFilesystem(FatFilesystem::new()),
        DiskInterface::Virtio,
        DiskBackend::InMemory { readonly: true },
        16,
    );

    cfg.boot_order(vec!["boot-disk", "unbootable"]);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    if !vm.guest_os_kind().is_linux() {
        phd_skip!("boot order tests require efivarfs to manipulate UEFI vars");
    }
    vm.launch().await?;
    vm.wait_to_boot().await?;

    // If the guest doesn't have an EFI partition then there's no way for boot
    // order preferences to be persisted.
    let mountline = vm.run_shell_command("mount | grep efivarfs").await?;

    if !mountline.starts_with("efivarfs on ") {
        warn!(
            "guest doesn't have an efivarfs, cannot manage boot order! \
            exiting test WITHOUT VALIDATING ANYTHING"
        );
        return Ok(());
    }

    // Try adding a few new boot options, then add them to the boot order,
    // reboot, and make sure they're all as we set them.
    if !vm
        .run_shell_command(&format!("ls {}", efipath(&bootvar(0xffff))))
        .await?
        .is_empty()
    {
        warn!(
            "guest environment already has a BootFFFF entry; \
            is this not a fresh image?"
        );
    }

    let boot_num: u16 = {
        let bytes = read_efivar(&vm, BOOT_CURRENT_VAR).await?;
        u16::from_le_bytes(bytes.try_into().unwrap())
    };

    // The entry we booted from is clearly valid, so we should be able to insert
    // a few duplicate entries. We won't boot into them, but if something bogus
    // happens and we did boot one of these, at least it'll work and we can
    // detect the misbehavior.
    //
    // But here's a weird one: if we just append these to the end, on reboot
    // they'll be moved somewhat up the boot order. This occurrs both if setting
    // variables through `efibootmgr` or by writing to
    // /sys/firmware/efi/efivars/BootOrder-* directly. As an example, say we had
    // a boot order of "0004,0001,0003,0000" where boot options were as follows:
    // * 0000: UiApp
    // * 0001: PCI device 4, function 0
    // * 0003: EFI shell (Firmware volume+file)
    // * 0004: Ubuntu (HD partition 15, GPT formatted)
    //
    // If we duplicate entry 4 to new options FFF0 and FFFF, reset the boot
    // order to "0004,0001,0003,0000,FFF0,FFFF", then reboot the VM, the boot
    // order when it comes back up will be "0004,0001,FFF0,FFFF,0003,0000".
    //
    // This almost makes sense, but with other devices in the mix I've seen
    // reorderings like `0004,0001,<PCI 16.0>,0003,0000,FFF0,FFFF` turning into
    // `0004,0001,FFF0,FFFF,<PCI 16.0>,0003,0000`. This is particularly strange
    // in that the new options were reordered around some other PCI device. It's
    // not the boot order we set!
    //
    // So, to at least confirm we *can* modify the boot order in a stable way,
    // make a somewhat less ambitious change: insert the duplicate boot options
    // in the order directly after the option they are duplicates of. This seems
    // to not get reordered.
    let boot_option_bytes = read_efivar(&vm, &bootvar(boot_num)).await?;

    // Finally, seeing a read-write `efivarfs` is not sufficient to know that
    // writes to EFI variables will actually stick. For example, an Alpine live
    // image backed by an ISO 9660 filesystem may have an EFI System Partition
    // and `efivarfs`, but certainly cannot persist state and will drop writes
    // to EFI variables.
    //
    // Check for this condition and exit early if the guest OS configuration
    // will not let us perform a useful test.
    write_efivar(&vm, &bootvar(0xfff0), &boot_option_bytes).await?;
    let reread = read_efivar(&vm, &bootvar(0xfff0)).await?;
    if reread.is_empty() {
        phd_skip!("Guest environment drops EFI variable writes");
    } else {
        assert_eq!(
            boot_option_bytes,
            read_efivar(&vm, &bootvar(0xfff0)).await?,
            "EFI variable write wrote something, but not what we expected?"
        );
    }

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
    vm.graceful_reboot().await?;

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

// This test is less demonstrating specific desired behavior, and more the
// observed behavior of OVMF with configuration we can offer today. If Propolis
// or other changes break this test, the test may well be what needs changing.
//
// If a `bootorder` file is present in fwcfg, there two relevant consequences
// demonstrated here: * The order of devices in `bootorder` is the order that
// will be used; on reboot any persisted configuration will be replaced with one
// derived from `bootorder` and corresponding OVMF logic.  * Guests cannot
// meaningfully change boot order. If an entry is in `bootorder`, that
// determines its' order. If it is not in `bootorder` but is retained for
// booting, it is appended to the end of the boot order in what seems to be the
// order that OVMF discovers the device.
//
// If `bootorder` is removed for subsequent reboots, the EFI System Partition's
// store of NvVar variables is the source of boot order, and guests can control
// their boot fates.
#[phd_testcase]
async fn boot_order_source_priority(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("boot_order_source_priority");

    cfg.data_disk(
        "unbootable",
        DiskSource::FatFilesystem(FatFilesystem::new()),
        DiskInterface::Virtio,
        DiskBackend::InMemory { readonly: true },
        16,
    );

    cfg.data_disk(
        "unbootable-2",
        DiskSource::FatFilesystem(FatFilesystem::new()),
        DiskInterface::Virtio,
        DiskBackend::InMemory { readonly: true },
        20,
    );

    // For the first stage of this test, we want to leave the boot procedure up
    // to whatever the guest firmware will do.
    cfg.clear_boot_order();

    let mut vm_no_bootorder = ctx.spawn_vm(&cfg, None).await?;
    if !vm_no_bootorder.guest_os_kind().is_linux() {
        phd_skip!("boot order tests require efivarfs to manipulate UEFI vars");
    }
    vm_no_bootorder.launch().await?;
    vm_no_bootorder.wait_to_boot().await?;

    let boot_option_numbers = discover_boot_option_numbers(
        &vm_no_bootorder,
        &[
            ((4, 0), "boot-disk"),
            ((16, 0), "unbootable"),
            ((20, 0), "unbootable-2"),
        ],
    )
    .await?;

    // `unbootable` should be somewhere in the middle of the boot order:
    // definitely between `boot-disk` and `unbootable-2`, for the options
    // enumerated from PCI devices.
    let unbootable_num = boot_option_numbers["unbootable"];

    let unbootable_idx = remove_boot_entry(&vm_no_bootorder, unbootable_num)
        .await?
        .expect("unbootable was in the boot order");

    vm_no_bootorder.graceful_reboot().await?;

    let reloaded_order = read_efivar(&vm_no_bootorder, BOOT_ORDER_VAR).await?;

    // Somewhat unexpected, but where OVMF gets us: `unbootable` is back in the
    // boot order, but at the end of the list. One might hope it would be
    // entirely removed from the boot order now, but no such luck. The good news
    // is that we can in fact influence the boot order.
    let unbootable_idx_after_reboot =
        find_option_in_boot_order(&reloaded_order, unbootable_num)
            .expect("unbootable is back in the order");

    let last_boot_option = &reloaded_order[reloaded_order.len() - 2..];
    assert_eq!(last_boot_option, &unbootable_num.to_le_bytes());

    // But this new position for `unbootable` definitely should be different
    // from before.
    assert_ne!(unbootable_idx, unbootable_idx_after_reboot);

    // And if we do the whole dance again with an explicit boot order provided
    // to the guest, we'll get different results!
    drop(vm_no_bootorder);
    cfg.boot_order(vec!["boot-disk", "unbootable", "unbootable-2"]);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let boot_option_numbers = discover_boot_option_numbers(
        &vm,
        &[
            ((4, 0), "boot-disk"),
            ((16, 0), "unbootable"),
            ((20, 0), "unbootable-2"),
        ],
    )
    .await?;

    let unbootable_num = boot_option_numbers["unbootable"];

    // Try removing a fw_cfg-defined boot option.
    let unbootable_idx = remove_boot_entry(&vm, unbootable_num)
        .await?
        .expect("unbootable was in the boot order");

    vm.graceful_reboot().await?;

    let reloaded_order = read_efivar(&vm, BOOT_ORDER_VAR).await?;

    // The option will be back in the boot order, where it was before! This is
    // because fwcfg still has a `bootorder` file.
    assert_eq!(
        find_option_in_boot_order(&reloaded_order, unbootable_num),
        Some(unbootable_idx)
    );
}

#[phd_testcase]
async fn nvme_boot_option_description(ctx: &Framework) {
    let mut cfg = ctx.vm_config_builder("nvme_boot_option_description");

    cfg.data_disk(
        "nvme-test-disk",
        DiskSource::Artifact(ctx.default_guest_os_artifact()),
        DiskInterface::Nvme,
        DiskBackend::File,
        8,
    );

    // We'll boot to `boot-disk`, but this test actually cares about the
    // description of `nvme-test-disk`. Ensure it's in the boot order list so
    // that we'll have a `BootNNNN` option for it.
    cfg.boot_order(vec!["boot-disk", "nvme-test-disk"]);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    if !vm.guest_os_kind().is_linux() {
        phd_skip!("boot option description test depends on efivarfs");
    }
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let boot_option_numbers = discover_boot_option_numbers(
        &vm,
        &[((4, 0), "boot-disk"), ((8, 0), "nvme-test-disk")],
    )
    .await?;

    let test_disk_option: u16 = boot_option_numbers["nvme-test-disk"];

    let test_disk_option_bytes =
        read_efivar(&vm, &bootvar(test_disk_option)).await?;

    let mut cursor = Cursor::new(test_disk_option_bytes.as_slice());

    let load_option = EfiLoadOption::parse_from(&mut cursor).unwrap();

    // Just a quick integrity check: we just put `nvme-test-disk` at PCI slot 8,
    // so we should be comparing to a load option describing PCI slot 8. If
    // these don't match, the description checking below would probably be a red
    // herring.
    assert!(load_option.path.matches_pci_device_function(8, 0));

    // The test assertion here is "UEFI  2" because we currently expect an NVMe
    // boot option to be named via the following procedure:
    // * fw_cfg bootorder (via `cfg_boot_order()` above) specifies `boot-disk`
    //   first, and `nvme-test-disk` second.
    // * OVMF processes boot options in that order. For each option:
    //   * try determining a boot description via these handlers in order:
    //     https://github.com/oxidecomputer/edk2/blob/propolis/edk2-stable202105/MdeModulePkg/Library/UefiBootManagerLib/BmBootDescription.c#L749-L756
    // * `boot-disk` is NVMe and described by BmGetNvmeDescription
    // * in that function, OVMF sends an NVMe IDENTIFY CONTROLLER command:
    //   https://github.com/oxidecomputer/edk2/blob/propolis/edk2-stable202105/MdeModulePkg/Library/UefiBootManagerLib/BmBootDescription.c#L600-L618
    // * the returned identification information has the following Mn/Sn:
    //   - Mn: default (`[0; 40]`):
    //     https://github.com/oxidecomputer/propolis/blob/5fe523a/lib/propolis/src/hw/nvme/bits.rs#L1001
    //   - Sn: the first 20 characters of the disk name. Here: "boot-disk"
    //     https://github.com/oxidecomputer/propolis/blob/5fe523a/lib/propolis/src/hw/nvme/mod.rs#L507-L532
    // * OVMF assembles the identification information into a wide-char string
    //   like "\x00\x00\x00\x00\x00... boot-disk\x00\x00...":
    //   https://github.com/oxidecomputer/edk2/blob/propolis/edk2-stable202105/MdeModulePkg/Library/UefiBootManagerLib/BmBootDescription.c#L628-L643
    // * The preliminary description has "UEFI " prepended to it:
    //   https://github.com/oxidecomputer/edk2/blob/propolis/edk2-stable202105/MdeModulePkg/Library/UefiBootManagerLib/BmBootDescription.c#L788-L790
    // * `StrCatS` appends the preliminary description to this new string.
    //   Because the model number is all nulls, the first character of
    //   "boot-disk"'s description is \x00, and `StrCatS` immediately returns
    //   having added nothing to the description:
    //   https://github.com/oxidecomputer/edk2/blob/propolis/edk2-stable202105/MdeModulePkg/Library/UefiBootManagerLib/BmBootDescription.c#L791
    // * At this point "boot-disk"'s description is "UEFI ". The same procedure
    //   runs for "nvme-test-disk" and describes it "UEFI " as well.
    // * Finally, `BmMakeBootOptionDescriptionUnique` runs and appends " 2" to
    //   make "nvme-test-disk"'s description distinct from "boot-disk". At this
    //   point, "nvme-test-disk"'s description is "UEFI  2":
    //   https://github.com/oxidecomputer/edk2/blob/propolis/edk2-stable202105/MdeModulePkg/Library/UefiBootManagerLib/BmBootDescription.c#L863-L868
    const EXPECTED_BOOT_DESCRIPTION: &str = "UEFI  2";

    // Hey! If this assertion failed, you may have done a good thing!
    //
    // This test's primary purpose is to ensure we do not *unknowingly* change
    // the description of OVMF-determined boot options. It is not unacceptable
    // that these options change, but changing them requires careful
    // consideration. Specifically, as of writing this test, if a device has:
    // * a boot option automatically determined by EDK2
    // * has that option persisted to NvVars
    // * a boot option with changed name on subsequent boot
    // the previously valid automatically-added boot option will be removed from
    // the boot order, and a new option with the new name will be added to the
    // end of the boot order.
    //
    // At this point, if the EFI shell is in the boot order list and in front of
    // a disk with a bootable OS on it, a guest VM could end up simply booting
    // into the EFI shell and get "stuck" there. This is not ideal, especially
    // since operating the EFI shell is not very well documented.
    //
    // So, if this assertion failed, something caused the
    // automatically-determined boot option description to change. You may be
    // providing a model number in the NVMe IDENTIFY CONTROLLER command, or OVMF
    // may be using different logic to determine descriptions. Presumably you've
    // changed something, so you'd have a better guess than me. If UEFI NvVars
    // are still retained in user-managed disks, where we are not managing the
    // ESP or NvVars data ourselves, then we probably should preserve existing
    // disk boot option descriptions. This test would be a great place to ensure
    // any new compatibility mechanism also works correctly. If UEFI NvVars are
    // provided through an emulated firmware device, or we're being more
    // invasive with changes to OVMF including boot order determination, then
    // maybe the assertion should fail and this test is no longer useful!
    assert_eq!(load_option.description, EXPECTED_BOOT_DESCRIPTION);
}
