// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! EFI variable parsing and manipulation utilities.
//!
//! Conceptually, this would be a separate crate. Something like `uefi`, or maybe more accurately,
//! `uefi-raw`. Those crates are very oriented towards *being* the platform firmware though - it's
//! not clear how to use them to parse a boot option into a device path, for example, though they
//! clearly are able to support processing device paths.
//!
//! So instead, this is enough supporting logic for our tests in Propolis.

use anyhow::{bail, Error};
use byteorder::{LittleEndian, ReadBytesExt};
use phd_testcase::*;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use tracing::{info, warn};

use super::run_long_command;

// First, some GUIDs. These GUIDs come from EDK2, and OVMF reuses them. Notably these are the raw
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
pub(crate) const EDK2_FIRMWARE_VOL_GUID: &'static [u8; 16] = &[
    0xc9, 0xbd, 0xb8, 0x7c, 0xeb, 0xf8, 0x34, 0x4f, 0xaa, 0xea, 0x3e, 0xe4,
    0xaf, 0x65, 0x16, 0xa1,
];
pub(crate) const EDK2_UI_APP_GUID: &'static [u8; 16] = &[
    0x21, 0xaa, 0x2c, 0x46, 0x14, 0x76, 0x03, 0x45, 0x83, 0x6e, 0x8a, 0xb6,
    0xf4, 0x66, 0x23, 0x31,
];
pub(crate) const EDK2_EFI_SHELL_GUID: &'static [u8; 16] = &[
    0x83, 0xa5, 0x04, 0x7c, 0x3e, 0x9e, 0x1c, 0x4f, 0xad, 0x65, 0xe0, 0x52,
    0x68, 0xd0, 0xb4, 0xd1,
];

// The variable namespace `8be4df61-93ca-11d2-aa0d-00e098032b8c` comes from UEFI, as do the
// variable names here. The presentation as `{varname}-{namespace}`, and at a path like
// `/sys/firmware/efi/efivars/`, are both Linux `efivars`-isms.
//
// These tests likely will not pass when run with other guest OSes.
pub(crate) const BOOT_CURRENT_VAR: &'static str =
    "BootCurrent-8be4df61-93ca-11d2-aa0d-00e098032b8c";
pub(crate) const BOOT_ORDER_VAR: &'static str =
    "BootOrder-8be4df61-93ca-11d2-aa0d-00e098032b8c";

pub(crate) fn bootvar(num: u16) -> String {
    format!("Boot{:04X}-8be4df61-93ca-11d2-aa0d-00e098032b8c", num)
}

pub(crate) fn efipath(varname: &str) -> String {
    format!("/sys/firmware/efi/efivars/{}", varname)
}

/// A (very limited) parse of an `EFI_LOAD_OPTION` descriptor.
#[derive(Debug)]
pub(crate) struct EfiLoadOption {
    pub description: String,
    pub path: EfiLoadPath,
}

#[derive(Debug)]
pub(crate) enum EfiLoadPath {
    Device { acpi_root: DevicePath, pci_device: DevicePath },
    FirmwareFile { volume: DevicePath, file: DevicePath },
}

impl EfiLoadPath {
    pub fn matches_fw_file(
        &self,
        fw_vol: &[u8; 16],
        fw_file: &[u8; 16],
    ) -> bool {
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

    pub fn matches_pci_device_function(
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

    pub fn as_pci_device_function(&self) -> Option<(u8, u8)> {
        if let EfiLoadPath::Device {
            acpi_root: DevicePath::Acpi { .. },
            pci_device: DevicePath::Pci { device, function },
        } = self
        {
            Some((*device, *function))
        } else {
            None
        }
    }
}

#[allow(dead_code)]
// The `Acpi` fields are not explicitly used (yet), but are useful for `Debug` purposes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DevicePath {
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
    pub(crate) fn parse_from(
        bytes: &mut Cursor<&[u8]>,
    ) -> Result<EfiLoadOption, Error> {
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
            .map_err(|e| {
                anyhow::anyhow!("unable to parse device path element: {:?}", e)
            })?;
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

    pub fn pci_device_function(&self) -> (u8, u8) {
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
    eprintln!("unhexing {}", s);
    let mut res = Vec::new();
    for chunk in s.as_bytes().chunks(2) {
        assert_eq!(chunk.len(), 2);

        let s = std::str::from_utf8(chunk).expect("valid string");

        let b = u8::from_str_radix(s, 16).expect("can parse");

        res.push(b);
    }
    return res;
}

/// Read the EFI variable `varname` from inside the VM, and return the data therein as a byte
/// array.
pub(crate) async fn read_efivar(
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
pub(crate) async fn write_efivar(
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

/// Learn the boot option numbers associated with various boot options that may or should
/// exist.
///
/// The fundamental wrinkle here is that we don't necessarily know what `Boot####` entries
/// exist, or which numbers they have, because NvVar is handled through persistence in guest
/// disks. This means a guest image may have some prior NvVar state with `Boot####` entries
/// that aren't removed, and cause entries reflecting the current system to have later numbers
/// than a fully blank initial set of variables.
pub(crate) async fn discover_boot_option_numbers(
    vm: &phd_framework::TestVm,
    device_names: &[((u8, u8), &'static str)],
) -> Result<HashMap<String, u16>> {
    let mut option_mappings: HashMap<String, u16> = HashMap::new();

    let boot_order_bytes = read_efivar(&vm, BOOT_ORDER_VAR).await?;
    warn!("initial boot order var: {:?}", boot_order_bytes);

    for chunk in boot_order_bytes.chunks(2) {
        assert_eq!(chunk.len(), 2);
        let option_num = u16::from_le_bytes(chunk.try_into().unwrap());

        let option_bytes = read_efivar(&vm, &bootvar(option_num)).await?;

        let mut cursor = Cursor::new(option_bytes.as_slice());

        let load_option = match EfiLoadOption::parse_from(&mut cursor) {
            Ok(option) => option,
            Err(e) => {
                warn!("unhandled boot option: {:?}", e);
                continue;
            }
        };

        if load_option
            .path
            .matches_fw_file(EDK2_FIRMWARE_VOL_GUID, EDK2_UI_APP_GUID)
        {
            let prev = option_mappings.insert("uiapp".to_string(), option_num);
            assert_eq!(prev, None);
        } else if load_option
            .path
            .matches_fw_file(EDK2_FIRMWARE_VOL_GUID, EDK2_EFI_SHELL_GUID)
        {
            let prev =
                option_mappings.insert("efi shell".to_string(), option_num);
            assert_eq!(prev, None);
        } else if let Some((device, function)) =
            load_option.path.as_pci_device_function()
        {
            let description = device_names.iter().find_map(|(path, desc)| {
                if path.0 == device && path.1 == function {
                    Some(desc)
                } else {
                    None
                }
            });

            if let Some(description) = description {
                option_mappings.insert(description.to_string(), option_num);
            } else {
                warn!("unknown PCI boot device {:#x}.{:#x}", device, function);
            }
        } else {
            warn!("unknown boot option: {:?}", load_option);

            let prev = option_mappings
                .insert(load_option.description.to_string(), option_num);
            assert_eq!(prev, None);
        }
    }

    info!("found boot options: {:?}", option_mappings);

    Ok(option_mappings)
}

pub(crate) fn find_option_in_boot_order(
    order: &[u8],
    option: u16,
) -> Option<usize> {
    let option = option.to_le_bytes();
    order
        .chunks(2)
        .enumerate()
        .find(|(_i, chunk)| *chunk == option)
        .map(|(i, _chunk)| i)
}

/// Remove the boot option from `vm`'s EFI BootOrder variable. `boot_option_num` is assumed to
/// refer to a boot option named like `format!("Boot{boot_option_num:4X}-*")`.
///
/// If the boot order was actually modified, returns the index that `boot_option_num` was
/// removed at.
pub(crate) async fn remove_boot_entry(
    vm: &phd_framework::TestVm,
    boot_option_num: u16,
) -> Result<Option<usize>> {
    let mut without_option = read_efivar(&vm, BOOT_ORDER_VAR).await?;

    let Some(option_idx) =
        find_option_in_boot_order(&without_option, boot_option_num)
    else {
        return Ok(None);
    };

    info!(
        "Removing Boot{:4X} from the boot order. It was at index {}",
        boot_option_num, option_idx
    );

    without_option.remove(option_idx * 2);
    without_option.remove(option_idx * 2);

    // Technically it's fine if an option is present multiple times, but typically an option is
    // present only once. This function intends to remove all copies of the specified option,
    // so assert that we have done so in the new order.
    assert_eq!(
        find_option_in_boot_order(&without_option, boot_option_num),
        None
    );

    write_efivar(&vm, BOOT_ORDER_VAR, &without_option).await?;

    Ok(Some(option_idx))
}

// And finally, actual test cases using the above!
