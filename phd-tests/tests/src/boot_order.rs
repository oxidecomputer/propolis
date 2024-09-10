// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::{bail, Error};
use byteorder::{LittleEndian, ReadBytesExt};
use phd_framework::{
    disk::{fat::FatFilesystem, DiskSource},
    test_vm::{DiskBackend, DiskInterface},
};
use phd_testcase::*;
use std::io::{Cursor, Read};
use tracing::{info, warn};

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

// That all said, while this test checks for specific EDK2/OVMF magic values, those magic values
// should be explained. The rest of this comment does just that.
//
// First, soem GUIDs. These GUIDs come from EDK2, and OVMF reuses them. Notably these are the raw
// bytes of the GUID: textual values will have slightly different ordering of bytes.
//
// Some source references, as you won't find these GUIDs in a UEFI or related spec document.. The
// firmware volume is identified by what seems to be the DXE firmware volume:
// https://github.com/tianocore/edk2/blob/712797c/OvmfPkg/OvmfPkgIa32.fdf#L181
// introduced in
// https://github.com/tianocore/edk2/commit/16f26de663967b5a64140b6abba2c145ea50194c, note this
// is the DXEFV entry.
//
// The *files* we'll care about in this test are identified by other GUIDs in the above *volume*.
//
// EFI Internal Shell: https://github.com/tianocore/edk2/blob/a445e1a/ShellPkg/ShellPkg.dec#L59-L60
// UiApp:
// https://github.com/tianocore/edk2/blob/a445e1a/MdeModulePkg/Application/UiApp/UiApp.inf#L13
const EDK2_FIRMWARE_VOL_GUID: &'static [u8; 16] = &[
    0xc9, 0xbd, 0xb8, 0x7c, 0xeb, 0xf8, 0x34, 0x4f, 0xaa, 0xea, 0x3e, 0xe4,
    0xaf, 0x65, 0x16, 0xa1,
];
const EDK2_UI_APP_GUID: &'static [u8; 16] = &[
    0x21, 0xaa, 0x2c, 0x46, 0x14, 0x76, 0x03, 0x45, 0x83, 0x6e, 0x8a, 0xb6,
    0xf4, 0x66, 0x23, 0x31,
];
const EDK2_EFI_SHELL_GUID: &'static [u8; 16] = &[
    0x83, 0xa5, 0x04, 0x7c, 0x3e, 0x9e, 0x1c, 0x4f, 0xad, 0x65, 0xe0, 0x52,
    0x68, 0xd0, 0xb4, 0xd1,
];

// The variable namespace `8be4df61-93ca-11d2-aa0d-00e098032b8c` comes from UEFI, as do the
// variable names here. The presentation as `{varname}-{namespace}`, and at a path like
// `/sys/firmware/efi/efivars/`, are both Linux `efivars`-isms.
//
// These tests likely will not pass when run with other guest OSes.
const BOOT_CURRENT_VAR: &'static str =
    "BootCurrent-8be4df61-93ca-11d2-aa0d-00e098032b8c";
const BOOT_ORDER_VAR: &'static str =
    "BootOrder-8be4df61-93ca-11d2-aa0d-00e098032b8c";

fn bootvar(num: u16) -> String {
    format!("Boot{:04X}-8be4df61-93ca-11d2-aa0d-00e098032b8c", num)
}

fn efipath(varname: &str) -> String {
    format!("/sys/firmware/efi/efivars/{}", varname)
}

/// A (very limited) parse of an `EFI_LOAD_OPTION` descriptor.
#[derive(Debug)]
struct EfiLoadOption {
    description: String,
    path: EfiLoadPath,
}

#[derive(Debug)]
enum EfiLoadPath {
    Device { acpi_root: DevicePath, pci_device: DevicePath },
    FirmwareFile { volume: DevicePath, file: DevicePath },
}

impl EfiLoadPath {
    fn matches_fw_file(&self, fw_vol: &[u8; 16], fw_file: &[u8; 16]) -> bool {
        if let EfiLoadPath::FirmwareFile {
            volume: DevicePath::FirmwareVolume { guid: vol_gid },
            file: DevicePath::FirmwareFile { guid: vol_file },
        } = self
        {
            vol_gid == fw_vol && vol_file == fw_file
        } else {
            false
        }
    }

    fn matches_pci_device_function(
        &self,
        pci_device: u8,
        pci_function: u8,
    ) -> bool {
        if let EfiLoadPath::Device {
            acpi_root: DevicePath::Acpi { .. },
            pci_device: DevicePath::Pci { device, function },
        } = self
        {
            pci_device == *device && pci_function == *function
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DevicePath {
    Acpi { hid: u32, uid: u32 },
    Pci { device: u8, function: u8 },

    // These two are described in sections 8.2 and 8.3 of the UEFI PI spec, respectively.
    // Version 1.6 can be found at https://uefi.org/sites/default/files/resources/PI_Spec_1.6.pdf
    FirmwareVolume { guid: [u8; 16] },
    FirmwareFile { guid: [u8; 16] },
}

impl DevicePath {
    fn parse_from(bytes: &mut Cursor<&[u8]>) -> Result<DevicePath, Error> {
        let ty = bytes.read_u8()?;
        let subtype = bytes.read_u8()?;

        match (ty, subtype) {
            (2, 1) => {
                // ACPI Device Path
                let size = bytes.read_u16::<LittleEndian>()?;
                if size != 0xc {
                    bail!("ACPI Device Path size is wrong (was {:#04x}, not 0x000c)", size);
                }
                let hid = bytes.read_u32::<LittleEndian>().unwrap();
                let uid = bytes.read_u32::<LittleEndian>().unwrap();
                Ok(DevicePath::Acpi { hid, uid })
            }
            (1, 1) => {
                // PCI device path
                let size = bytes.read_u16::<LittleEndian>()?;
                if size != 0x6 {
                    bail!("PCI Device Path size is wrong (was {:#04x}, not 0x0006)", size);
                }
                let function = bytes.read_u8().unwrap();
                let device = bytes.read_u8().unwrap();
                Ok(DevicePath::Pci { device, function })
            }
            (4, 6) => {
                // "PIWG Firmware File" aka "Firmware File" in UEFI PI spec
                let size = bytes.read_u16::<LittleEndian>()?;
                if size != 0x14 {
                    bail!(
                        "Firmware File size is wrong (was {:#04x}, not 0x0014)",
                        size
                    );
                }
                let mut guid = [0u8; 16];
                bytes.read_exact(&mut guid)?;
                Ok(DevicePath::FirmwareFile { guid })
            }
            (4, 7) => {
                // "PIWG Firmware Volume" aka "Firmware Volume" in UEFI PI spec
                let size = bytes.read_u16::<LittleEndian>()?;
                if size != 0x14 {
                    bail!("Firmware Volume size is wrong (was {:#04x}, not 0x0014)", size);
                }
                let mut guid = [0u8; 16];
                bytes.read_exact(&mut guid)?;
                Ok(DevicePath::FirmwareVolume { guid })
            }
            (ty, subtype) => {
                bail!(
                    "Device path type/subtype unsupported: ({:#02x}/{:#02x})",
                    ty,
                    subtype
                );
            }
        }
    }
}

impl EfiLoadOption {
    // parsing here brought to you by rereading
    // * https://uefi.org/specs/UEFI/2.10/10_Protocols_Device_Path_Protocol.html
    // * https://uefi.org/specs/UEFI/2.10/03_Boot_Manager.html
    fn parse_from(bytes: &mut Cursor<&[u8]>) -> Result<EfiLoadOption, Error> {
        let _attributes = bytes.read_u32::<LittleEndian>()?;
        let file_path_list_length = bytes.read_u16::<LittleEndian>()?;

        // The `Description` field is a null-terminated string of char16.
        let mut description_chars: Vec<u16> = Vec::new();

        loop {
            let c = bytes.read_u16::<LittleEndian>()?;
            description_chars.push(c);
            if c == 0 {
                break;
            }
        }

        let description = String::from_utf16(&description_chars)
            .expect("description is valid utf16");

        let mut device_path_cursor = Cursor::new(
            &bytes.get_ref()[bytes.position() as usize..]
                [..file_path_list_length as usize],
        );

        let path_entry = DevicePath::parse_from(&mut device_path_cursor)
            .expect("can read device path element");
        let load_path = match path_entry {
            acpi_root @ DevicePath::Acpi { .. } => {
                let pci_device =
                    DevicePath::parse_from(&mut device_path_cursor)
                        .expect("can read device path element");
                if !matches!(pci_device, DevicePath::Pci { .. }) {
                    bail!("expected ACPI Device Path entry to be followed by a PCI Device Path, but was {:?}", pci_device);
                }

                EfiLoadPath::Device { acpi_root, pci_device }
            }
            volume @ DevicePath::FirmwareVolume { .. } => {
                let file = DevicePath::parse_from(&mut device_path_cursor)
                    .expect("can read device path element");
                if !matches!(file, DevicePath::FirmwareFile { .. }) {
                    bail!("expected Firmware Volume entry to be followed by a Firmware File, but was {:?}", file);
                }

                EfiLoadPath::FirmwareFile { volume, file }
            }
            other => {
                bail!("unexpected root EFI Load Option path item: {:?}", other);
            }
        };

        // Not strictly necessary, but advance `bytes` by the number of bytes we read from
        // `device_path_cursor`. To callers, this keeps it as if we had just been reading `bytes`
        // all along.
        bytes.set_position(bytes.position() + device_path_cursor.position());

        Ok(EfiLoadOption { description, path: load_path })
    }

    fn pci_device_function(&self) -> (u8, u8) {
        let EfiLoadPath::Device {
            pci_device: DevicePath::Pci { device, function },
            ..
        } = self.path
        else {
            panic!(
                "expected load path to be an ACPI/PCI pair, but was {:?}",
                self.path
            );
        };
        (device, function)
    }
}

fn unhex(s: &str) -> Vec<u8> {
    let s = s.replace("\n", "");
    let mut res = Vec::new();
    for chunk in s.as_bytes().chunks(2) {
        assert_eq!(chunk.len(), 2);

        let s = std::str::from_utf8(chunk).expect("valid string");

        let b = u8::from_str_radix(s, 16).expect("can parse");

        res.push(b);
    }
    return res;
}

async fn run_long_command(
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

/// Read the EFI variable `varname` from inside the VM, and return the data therein as a byte
/// array.
async fn read_efivar(
    vm: &phd_framework::TestVm,
    varname: &str,
) -> Result<Vec<u8>, Error> {
    // Linux's `efivarfs` prepends 4 bytes of attributes to EFI variables.
    let cmd = format!(
        "dd status=none if={} bs=1 skip=4 | xxd -p -",
        efipath(varname)
    );

    let hex = run_long_command(vm, &cmd).await?;

    Ok(unhex(&hex))
}

/// Write the provided bytes to the EFI variable `varname`.
///
/// For Linux guests: variables automatically have their prior attributes prepended. Provide only
/// the variable's data.
async fn write_efivar(
    vm: &phd_framework::TestVm,
    varname: &str,
    data: &[u8],
) -> Result<(), Error> {
    let attr_cmd = format!(
        "dd status=none if={} bs=1 count=4 | xxd -p -",
        efipath(varname)
    );

    let attr_read_bytes = run_long_command(vm, &attr_cmd).await?;
    let attrs = if attr_read_bytes.ends_with(": No such file or directory") {
        // Default attributes if the variable does not exist yet. We expect it to be non-volatile
        // because we are writing it, we expect it to be available to boot services (not strictly
        // required, but for boot configuration we need it), and we expect it to be available at
        // runtime (e.g. where we are reading and writing it).
        //
        // so:
        // NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS
        const FRESH_ATTRS: u32 = 0x00_00_00_07;
        FRESH_ATTRS.to_le_bytes().to_vec()
    } else {
        unhex(&attr_read_bytes)
    };

    let mut new_value = attrs;
    new_value.extend_from_slice(data);

    // The command to write this data back out will be, roughtly:
    // ```
    // printf "\xAA\xAA\xAA\xAA\xDD\xDD\xDD\xDD" > /sys/firmware/efi/efivars/...
    // ```
    // where AAAAAAAA are the attribute bytes and DDDDDDDD are caller-provided data.
    let escaped: String =
        new_value.into_iter().map(|b| format!("\\x{:02x}", b)).collect();

    let cmd = format!("printf \"{}\" > {}", escaped, efipath(varname));

    let res = run_long_command(vm, &cmd).await?;
    // If something went sideways and the write failed with something like `invalid argument`...
    if res.len() != 0 {
        bail!("writing efi produced unexpected output: {}", res);
    }

    Ok(())
}

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

    cfg.boot_order(vec!["alpine-3-16", "alpine-3-20"]);

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
    assert!(load_option.path.matches_pci_device_function(16, 0));
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
