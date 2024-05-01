// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::common::*;
use crate::firmware::smbios::bits::{self, RawTable};
use crate::firmware::smbios::{Handle, SmbString};

pub trait Table {
    fn render(&self, handle: Handle) -> Vec<u8>;
}

#[derive(Default)]
pub struct Type0 {
    pub vendor: SmbString,
    pub bios_version: SmbString,
    pub bios_starting_seg_addr: u16,
    pub bios_release_date: SmbString,
    pub bios_rom_size: u8,
    /// The low 32 bits of the BIOS characteristics field is a set of bitflags
    /// that describes the BIOS.
    pub bios_characteristics: type0::BiosCharacteristics,
    /// The high 32 bits of the 64-bit BIOS characteristics field is reserved
    /// for the BIOS vendor.
    pub bios_characteristics_reserved: u32,
    pub bios_ext_characteristics: type0::BiosExtCharacteristics,
    pub bios_major_release: u8,
    pub bios_minor_release: u8,
    pub ec_firmware_major_rel: u8,
    pub ec_firmware_minor_rel: u8,
}

impl Table for Type0 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let bios_characteristics = {
            let low = u64::from(self.bios_characteristics.bits());
            let high = u64::from(self.bios_characteristics_reserved) << 32;
            low | high
        };
        let mut stab = StringTable::new();
        let data = bits::Type0 {
            vendor: stab.add(&self.vendor),
            bios_version: stab.add(&self.bios_version),
            bios_starting_seg_addr: self.bios_starting_seg_addr,
            bios_release_date: stab.add(&self.bios_release_date),
            bios_rom_size: self.bios_rom_size,
            bios_characteristics,
            bios_ext_characteristics: self.bios_ext_characteristics.bits(),
            bios_major_release: self.bios_major_release,
            bios_minor_release: self.bios_minor_release,
            ec_firmware_major_rel: self.ec_firmware_major_rel,
            ec_firmware_minor_rel: self.ec_firmware_minor_rel,
            ..bits::Type0::new(handle.into())
        };

        render_table(data, None, Some(stab))
    }
}

macro_rules! enum_try_from {
    ($(#[$attr:meta])* $v:vis enum $Enum:ident: $repr:ty {
        $(#[$variant1_attr:meta])*
        $Variant1:ident = $val1:expr,
        $(
            $(#[$variant_attr:meta])*
            $Variant:ident = $val:expr
        ),*
        $(,)?
    }) => {
        $(#[$attr])*
        #[repr($repr)]
        $v enum $Enum {
            $(#[$variant1_attr])*
            $Variant1 = $val1,
            $(
                $(#[$variant_attr])*
                $Variant = $val
            ),*
        }

        #[automatically_derived]
        impl std::convert::TryFrom<$repr> for $Enum {
            type Error = &'static str;
            fn try_from(value: $repr) -> Result<Self, Self::Error> {
                match value {
                    $val1 => Ok($Enum::$Variant1),
                    $(
                        $val => Ok($Enum::$Variant),
                    )+
                    _ => Err(concat!(
                        "invalid ", stringify!($enum), " value, expected one of: ",
                        stringify!($Variant1)," (", stringify!($val1), ")",
                        $(", ", stringify!($Variant), " (", stringify!($val), ")" ),*
                    )),
                }
            }
        }

        #[automatically_derived]
        impl From<$Enum> for $repr {
            fn from(e: $Enum) -> $repr {
                e as $repr
            }
        }

        #[automatically_derived]
        impl<'de> serde::de::Deserialize<'de> for $Enum {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::de::Deserializer<'de>,
                $repr: serde::de::Deserialize<'de>,
            {
                use serde::de::{Deserialize, Error, Unexpected};

                let value = <$repr as Deserialize>::deserialize(deserializer)?;
                $Enum::try_from(value)
                    .map_err(|e| Error::invalid_value(
                        Unexpected::Unsigned(value as u64),
                        &e,
                    ))
            }
        }

        #[automatically_derived]
        impl serde::ser::Serialize for $Enum {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::ser::Serializer,
                $repr: serde::ser::Serialize,
            {
                (*self as $repr).serialize(serializer)
            }
        }

        impl $Enum {
            // Every enum should have this IMO...
            pub const VARIANTS: &'static [Self] = &[
                $Enum::$Variant1,
                $($Enum::$Variant),*
            ];
        }
    };
}

macro_rules! serialize_bitflags {
    ($($Flags:ty => $repr:ty),+ $(,)?) => {
        $(

            #[automatically_derived]
            impl<'de> serde::de::Deserialize<'de> for $Flags {
                fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                where
                    D: serde::de::Deserializer<'de>,
                    $repr: serde::de::Deserialize<'de>,
                {
                    let v = <$repr as serde::de::Deserialize>::deserialize(deserializer)?;
                    <$Flags>::from_bits(v).ok_or_else(||
                        serde::de::Error::custom(format!(
                            "invalid {} value {v}: only bits {:?} may be set",
                            stringify!($Flags),
                            <$Flags>::all(),
                        ))
                    )
                }
            }

            #[automatically_derived]
            impl serde::ser::Serialize for $Flags {
                fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                where
                    S: serde::ser::Serializer,
                    $repr: serde::ser::Serialize,
                {
                    self.bits().serialize(serializer)
                }
            }
        )+
    };
}

pub mod type0 {
    bitflags! {
        /// BIOS Characteristics flags.
        ///
        /// See Table 7 in section 7.1.1 of [the SMBIOS Reference
        /// Specification][DSP0136] for details.
        ///
        /// [DSP0136]: https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.7.0.pdf
        #[repr(transparent)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct BiosCharacteristics: u32 {
            // Bits 0-1 are reserved.

            /// BIOS characteristics are unknown.
            const UNKNOWN = 1 << 2;
            /// BIOS characteristics are not supported.
            const UNSUPPORTED = 1 << 3;
            /// ISA is supported
            const ISA = 1 << 4;
            /// MCA is supported.
            const MCA = 1 << 5;
            /// EISA is supported.
            const EISA = 1 << 6;
            /// PCI is supported.
            const PCI = 1 << 7;
            /// PC card (PCMCIA) is supported.
            const PCMCIA = 1 << 8;
            /// Plug and Play is supported.
            const PLUG_AND_PLAY = 1 << 9;
            /// APM is supported.
            const APM = 1 << 10;
            /// BIOS is upgradeable (flash).
            const UPGRADEABLE = 1 << 11;
            /// BIOS shadowing is allowed.
            const SHADOWING = 1 << 12;
            /// VL-VESA is supported.
            const VL_VESA = 1 << 13;
            /// ESCD support is available.
            const ESCD = 1 << 14;
            /// Boot from CD is supported.
            const BOOT_FROM_CD = 1 << 15;
            /// Selectable boot is supported.
            const BOOT_SELECTABLE = 1 << 16;
            /// BIOS ROM is socketed (e.g. PLCC or SOP socket).
            const ROM_SOCKETED = 1 << 17;
            /// Boot from PC card (PCMCIA) is supported.
            const BOOT_FROM_PCMCIA = 1 << 18;
            /// EDD specification is supported.
            const EDD = 1 << 19;
            /// INT 0x13 --- Japanese floppy for NEC 9800 1.2 MB (3.5”, 1K
            /// bytes/sector, 360 RPM) is supported.
            const FLOPPY_NEC_9800 = 1 << 20;
            /// INT 0x13 --- Japanese floppy for Toshiba 1.2 MB (3.5”, 360
            const FLOPPY_TOSHIBA= 1 << 21;
            /// INT 0x13 --- 5.25”/360 KB floppy services are supported.
            const FLOPPY_5_25_IN_360KB = 1 << 22;
            /// INT 0x13 --- 5.25”/1.2 MB floppy services are supported.
            const FLOPPY_5_25_IN_1_2MB = 1 << 23;
            /// INT 0x13 --- 3.5”/720 KB floppy services are supported.
            const FLOPPY_3_5_IN_720KB = 1 << 24;
            /// INT 0x13 --- 3.5”/2.88 MB floppy services are supported.
            const FLOPPY_3_5_IN_2_88MB = 1 << 25;
            /// INT 0x5, print screen service is supported.
            const PRINT_SCREEN = 1 << 26;
            /// INT 0x9, 8042 keyboard services are supported.
            const KEYBOARD_8042 = 1 << 27;
            /// INT 0x14, serial services are supported.
            const SERIAL = 1 << 28;
            /// INT 0x17, printer services are supported.
            const PRINTER = 1 << 29;
            /// INT 0x10, CGA/mono video services are supported.
            const VIDEO_CGA_MONO = 1 << 30;
            /// NEC PC-98
            const NEC_PC_98 = 1 << 31;
        }
    }

    impl Default for BiosCharacteristics {
        fn default() -> Self {
            BiosCharacteristics::UNKNOWN
        }
    }

    bitflags! {
        /// BIOS Characteristics Extension flags.
        ///
        /// See Tables 8 and 9 in section 7.1.1 of [the SMBIOS Reference
        /// Specification][DSP0136] for details.
        ///
        /// [DSP0136]: https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.7.0.pdf
        #[repr(transparent)]
        #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct BiosExtCharacteristics: u16 {
            /// ACPI is supported
            const ACPI = 1 << 0;
            /// USB Legacy is supported.
            const USB_LEGACY = 1 << 1;
            /// AGP is supported.
            const AGP = 1 << 2;
            /// I2O boot is supported.
            const BOOT_I2O = 1 << 3;
            /// LS-120 SuperDisk boot is supported.
            const BOOT_LS_120_SUPERDISK = 1 << 4;
            /// ATAPI ZIP drive boot is supported.
            const BOOT_ATAPI_ZIP = 1 << 5;
            /// 1394 boot is supported.
            const BOOT_1394 = 1 << 6;
            /// Smart battery is supported.
            const SMART_BATTERY = 1 << 7;
            /// BIOS boot specification is supported.
            const BIOS_BOOT_SPEC = 1 << 8;
            /// Function key-initiated network service boot is supported.
            ///
            /// When function key-uninitiated network service boot is not supported,
            /// a network adapter option ROM may choose to offer this functionality
            /// on its own, thus offering this capability to legacy systems. When
            /// the function is supported, the network adapter option ROM shall not
            /// offer this capability.
            const NETBOOT_FN_KEY = 1 << 9;
            /// Enable targeted content distribution.
            ///
            /// The manufacturer has ensured that the SMBIOS data is useful in
            /// identifying the computer for targeted delivery of model-specific
            /// software and firmware content through third-party content
            /// distribution services.
            const TARGETED_CONTENT_DIST = 1 << 10;
            /// UEFI specification is supported.
            const UEFI = 1 << 11;
            /// SMBIOS table describes a virtual machine.
            ///
            /// If this bit is not set, no inference can be made about the
            /// virtuality of the system.
            const IS_VM = 1 << 12;
            /// Manufacturing mode is *supported*.
            ///
            /// Manufacturing mode is a special boot mode, not normally available to
            /// end users, that modifies BIOS features and settings for use while
            /// the computer is being manufactured and tested.
            const HAS_MFG_MODE = 1 << 13;
            /// Manufacturing mode is *enabled*.
            const IN_MFG_MODE = 1 << 14;
        }
    }

    serialize_bitflags! {
        BiosCharacteristics => u32,
        BiosExtCharacteristics => u16,
    }
}
#[derive(Default)]
pub struct Type1 {
    pub manufacturer: SmbString,
    pub product_name: SmbString,
    pub version: SmbString,
    pub serial_number: SmbString,
    pub uuid: [u8; 16],
    pub wake_up_type: type1::WakeUpType,
    pub sku_number: SmbString,
    pub family: SmbString,
}
impl Table for Type1 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let mut stab = StringTable::new();
        let data = bits::Type1 {
            manufacturer: stab.add(&self.manufacturer),
            product_name: stab.add(&self.product_name),
            version: stab.add(&self.version),
            serial_number: stab.add(&self.serial_number),
            uuid: self.uuid,
            wake_up_type: self.wake_up_type as u8,
            sku_number: stab.add(&self.sku_number),
            family: stab.add(&self.family),
            ..bits::Type1::new(handle.into())
        };

        render_table(data, None, Some(stab))
    }
}

pub mod type1 {
    enum_try_from! {
        /// Wake-up type.
        ///
        /// See Table 12 in section 7.2.2 of [the SMBIOS Reference
        /// Specification][DSP0136] for details.
        ///
        /// [DSP0136]:
        ///     https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.7.0.pdf
        #[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
        pub enum WakeUpType: u8 {
            /// Other
            Other = 0x1,
            /// Unknown
            #[default]
            Unknown = 0x2,
            /// APM Timer
            ApmTimer = 0x3,
            /// Modem Ring
            ModemRing = 0x4,
            /// LAN Remote
            LanRemote = 0x5,
            /// Power Switch
            PowerSwitch = 0x6,
            /// PCI PME#
            PciPme = 0x7,
            /// AC Power Restored
            AcPowerRestored = 0x8,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn wake_up_type_serde() {
            for variant in WakeUpType::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(dbg!(variant))).unwrap();
                let deserialized =
                    dbg!(serde_json::from_str::<'_, WakeUpType>(&serialized))
                        .unwrap();
                assert_eq!(*variant, deserialized);
            }
        }

        #[test]
        fn wake_up_type_deserialize() {
            for variant in WakeUpType::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(&(*dbg!(variant) as u8)))
                        .unwrap();
                let deserialized =
                    dbg!(serde_json::from_str::<'_, WakeUpType>(&serialized))
                        .unwrap();
                assert_eq!(*variant, deserialized);
            }

            for invalid in [0x9, 0xff, 0x7890] {
                let serialized = dbg!(serde_json::to_string(&invalid)).unwrap();

                dbg!(serde_json::from_str::<'_, WakeUpType>(&serialized))
                    .unwrap_err();
            }
        }
    }
}

#[derive(Default)]
pub struct Type4 {
    pub socket_designation: SmbString,
    pub proc_type: type4::ProcType,
    pub proc_family: u8,
    pub proc_manufacturer: SmbString,
    pub proc_id: u64,
    pub proc_version: SmbString,
    pub voltage: u8,
    pub external_clock: u16,
    pub max_speed: u16,
    pub current_speed: u16,
    pub status: type4::ProcStatus,
    pub proc_upgrade: u8,
    pub l1_cache_handle: Handle,
    pub l2_cache_handle: Handle,
    pub l3_cache_handle: Handle,
    pub serial_number: SmbString,
    pub asset_tag: SmbString,
    pub part_number: SmbString,
    pub core_count: u8,
    pub core_enabled: u8,
    pub thread_count: u8,
    pub proc_characteristics: type4::Characteristics,
    pub proc_family2: u16,
}
impl Type4 {
    pub fn set_family(&mut self, family: u16) {
        if family > 0xff {
            self.proc_family = 0xfe;
            self.proc_family2 = family;
        } else {
            self.proc_family = family as u8;
            self.proc_family2 = 0;
        }
    }
}
impl Table for Type4 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let mut stab = StringTable::new();
        let data = bits::Type4 {
            socket_designation: stab.add(&self.socket_designation),
            proc_type: self.proc_type as u8,
            proc_family: self.proc_family,
            proc_manufacturer: stab.add(&self.proc_manufacturer),
            proc_id: self.proc_id,
            proc_version: stab.add(&self.proc_version),
            voltage: self.voltage,
            external_clock: self.external_clock,
            max_speed: self.max_speed,
            current_speed: self.current_speed,
            status: self.status as u8,
            proc_upgrade: self.proc_upgrade,
            l1_cache_handle: self.l1_cache_handle.into(),
            l2_cache_handle: self.l2_cache_handle.into(),
            l3_cache_handle: self.l3_cache_handle.into(),
            serial_number: stab.add(&self.serial_number),
            asset_tag: stab.add(&self.asset_tag),
            part_number: stab.add(&self.part_number),
            core_count: self.core_count,
            core_enabled: self.core_enabled,
            thread_count: self.thread_count,
            proc_characteristics: self.proc_characteristics.bits(),
            proc_family2: self.proc_family2,
            ..bits::Type4::new(handle.into())
        };
        render_table(data, None, Some(stab))
    }
}

pub mod type4 {
    enum_try_from! {
        /// Processor type.
        ///
        /// See Table 21 in section 7.5 of [the SMBIOS Reference
        /// Specification][DSP0136] for details.
        ///
        /// [DSP0136]:
        ///     https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.7.0.pdf
        #[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
        pub enum ProcType: u8 {
            /// Other
            Other = 0x01,
            /// Unknown
            #[default]
            Unknown = 0x02,
            /// Central Processor
            Central = 0x03,
            /// Math Processor
            Math = 0x04,
            /// DSP Processor
            Dsp = 0x05,
            /// Video processor
            Video = 0x06,
        }
    }

    enum_try_from! {
        /// Processor status.
        ///
        /// See Table 21 in section 7.5 of [the SMBIOS Reference
        /// Specification][DSP0136] for details.
        ///
        /// [DSP0136]:
        ///     https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.7.0.pdf
        #[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
        pub enum ProcStatus: u8 {
            /// Status unknown, socket unpopulated.
            UnknownUnpopulated = 0b0000_0000,
            /// Status unknown, socket populated.
            #[default]
            UnknownPopulated = 0b0100_0000,

            /// CPU Enabled
            ///
            /// It...probably doesn't make sense to have a CPU enabled that's
            /// unpopulated?
            Enabled = 0b0100_0001,
            /// CPU Disabled by User through BIOS Setup.
            UserDisabled = 0b0100_0010,
            /// CPU Disabled by BIOS (POST Error).
            BiosDisabled = 0b0100_0011,
            /// CPU is Idle, waiting to be enabled.
            Idle = 0b0100_0100,

            /// Other
            OtherPopulated = 0b0100_0111,
            OtherUnpopulated = 0b0000_0111,
        }
    }

    impl ProcStatus {
        const POPULATED: u8 = 1 << 6;

        pub fn is_populated(&self) -> bool {
            (*self as u8) & Self::POPULATED != 0
        }
    }

    bitflags! {
        /// Processor characteristics.
        ///
        /// See Table 27 in section 7.5.9 of [the SMBIOS Reference
        /// Specification][DSP0136] for details.
        ///
        /// [DSP0136]:
        ///     https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.7.0.pdf
        #[repr(transparent)]
        #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct Characteristics: u16 {
            // Bit 0 is reserved

            /// Unknown
            const UNKNOWN = 1 << 1;
            /// 64-bit Capable
            const IS_64_BIT = 1 << 2;
            /// Multi-core
            const MULTI_CORE = 1 << 3;
            /// Hardware Thread
            const HARDWARE_THREAD = 1 << 4;
            /// Execute Protection
            const EXECUTE_PROTECTION = 1 << 5;
            /// Enhanced Virtualization
            const VIRTUALIZATION = 1 << 6;
            /// Power/Performance Control
            const POWER_PERF_CONTROL = 1 << 7;
            /// 128-bit Capable
            const IS_128_BIT = 1 << 8;
            /// Arm64 SoC ID
            const ARM64_SOC_ID = 1 << 9;
        }
    }

    serialize_bitflags! {
        Characteristics => u16,
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn proc_status_serde_roundtrip() {
            for variant in ProcStatus::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(dbg!(variant))).unwrap();
                let deserialized =
                    dbg!(serde_json::from_str::<'_, ProcStatus>(&serialized))
                        .unwrap();
                assert_eq!(*variant, deserialized);
            }
        }

        #[test]
        fn proc_status_deserialize() {
            for variant in ProcStatus::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(&(*dbg!(variant) as u8)))
                        .unwrap();
                let deserialized =
                    dbg!(serde_json::from_str::<'_, ProcStatus>(&serialized))
                        .unwrap();
                assert_eq!(*variant, deserialized);
            }

            for invalid in [0x9, 0xff, 0x7890] {
                let serialized = dbg!(serde_json::to_string(&invalid)).unwrap();

                dbg!(serde_json::from_str::<'_, ProcStatus>(&serialized))
                    .unwrap_err();
            }
        }

        #[test]
        fn proc_type_serde_roundtrip() {
            for variant in ProcType::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(dbg!(variant))).unwrap();
                let deserialized =
                    dbg!(serde_json::from_str::<'_, ProcType>(&serialized))
                        .unwrap();
                assert_eq!(*variant, deserialized);
            }
        }

        #[test]
        fn proc_type_deserialize() {
            for variant in ProcType::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(&(*dbg!(variant) as u8)))
                        .unwrap();
                let deserialized =
                    dbg!(serde_json::from_str::<'_, ProcType>(&serialized))
                        .unwrap();
                assert_eq!(*variant, deserialized);
            }

            for invalid in [0x9, 0xff, 0x7890] {
                let serialized = dbg!(serde_json::to_string(&invalid)).unwrap();

                dbg!(serde_json::from_str::<'_, ProcType>(&serialized))
                    .unwrap_err();
            }
        }
    }
}

#[derive(Default)]
pub struct Type16 {
    pub location: type16::Location,
    pub array_use: type16::ArrayUse,
    pub error_correction: type16::ErrorCorrection,
    pub max_capacity: u32,
    pub error_info_handle: Handle,
    pub num_mem_devices: u16,
    pub extended_max_capacity: u64,
}
impl Type16 {
    pub fn set_max_capacity(&mut self, capacity_bytes: usize) {
        let capacity_kib = capacity_bytes / KB;

        if capacity_bytes >= (2 * TB) {
            self.max_capacity = 0x8000_0000;
            self.extended_max_capacity = capacity_kib as u64;
        } else {
            self.max_capacity = capacity_kib as u32;
        }
    }
}
impl Table for Type16 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let data = bits::Type16 {
            location: self.location as u8,
            array_use: self.array_use as u8,
            error_correction: self.error_correction as u8,
            max_capacity: self.max_capacity,
            error_info_handle: self.error_info_handle.into(),
            num_mem_devices: self.num_mem_devices,
            extended_max_capacity: self.extended_max_capacity,
            ..bits::Type16::new(handle.into())
        };
        render_table(data, None, None)
    }
}

pub mod type16 {
    enum_try_from! {
        /// Memory array location.
        ///
        /// See Table 72 in section 7.17.1 of [the SMBIOS Reference
        /// Specification][DSP0136] for details.
        ///
        /// [DSP0136]:
        ///     https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.7.0.pdf
        #[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
        pub enum Location: u8 {
            /// Other
            Other = 0x01,
            /// Unknown
            #[default]
            Unknown = 0x02,
            /// System board or motherboard
            SystemBoard = 0x03,
            /// ISA add-on card
            IsaCard = 0x04,
            /// EISA add-on card
            EisaCard = 0x05,
            /// PCI add-on card
            PciCard = 0x06,
            /// MCA add-on card
            McaCard = 0x07,
            /// PCMCIA add-on card
            PcmciaCard = 0x08,
            /// Proprietary add-on card
            ProprietaryCard = 0x09,
            /// NuBus
            NuBus = 0x0A,
            /// PC-98/C20 add-on card
            Pc98C20Card = 0xA0,
            /// PC-98/C24 add-on card
            Pc98C24Card = 0xA1,
            /// PC-98/E  add-on card
            Pc98ECard = 0xA2,
            /// PC-98/Local bus add-on card
            Pc98LocalCard = 0xA3,
            // CXL add-on card
            CxlCard = 0xA4,
        }
    }

    enum_try_from! {
        /// Memory array use field.
        ///
        /// See Table 73 in section 7.17.2 of [the SMBIOS Reference
        /// Specification][DSP0136] for details.
        ///
        /// [DSP0136]:
        ///     https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.7.0.pdf
        #[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
        pub enum ArrayUse: u8 {
            /// Other
            Other = 0x1,
            /// Unknown
            #[default]
            Unknown = 0x2,
            /// System memory
            System = 0x3,
            /// Video memory
            Video = 0x4,
            /// Flash memory
            Flash = 0x5,
            /// Non-volatile RAM
            NonVolatile = 0x6,
            /// Cache memory
            Cache = 0x7,
        }
    }

    enum_try_from! {
        /// Memory array error correction field.
        ///
        /// See Table 74 in section 7.17.3 of [the SMBIOS Reference
        /// Specification][DSP0136] for details.
        ///
        /// [DSP0136]:
        ///     https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.7.0.pdf
        #[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
        pub enum ErrorCorrection: u8 {
            /// Other
            Other = 0x1,
            /// Unknown
            #[default]
            Unknown = 0x2,
            /// No error correction.
            None = 0x3,
            /// Parity
            Parity = 0x4,
            /// Single-bit ECC
            SingleBitEcc = 0x5,
            /// Multi-bit ECC
            MultiBitEcc = 0x6,
            /// CRC
            Crc = 0x7,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn location_serde_roundtrip() {
            for variant in Location::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(dbg!(variant))).unwrap();
                let deserialized =
                    dbg!(serde_json::from_str::<'_, Location>(&serialized))
                        .unwrap();
                assert_eq!(*variant, deserialized);
            }
        }

        #[test]
        fn location_deserialize() {
            for variant in Location::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(&(*dbg!(variant) as u8)))
                        .unwrap();
                let deserialized =
                    dbg!(serde_json::from_str::<'_, Location>(&serialized))
                        .unwrap();
                assert_eq!(*variant, deserialized);
            }

            for invalid in [0x11, 0xff, 0x7890] {
                let serialized = dbg!(serde_json::to_string(&invalid)).unwrap();

                dbg!(serde_json::from_str::<'_, Location>(&serialized))
                    .unwrap_err();
            }
        }

        #[test]
        fn array_use_serde_roundtrip() {
            for variant in ArrayUse::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(dbg!(variant))).unwrap();
                let deserialized =
                    dbg!(serde_json::from_str::<'_, ArrayUse>(&serialized))
                        .unwrap();
                assert_eq!(*variant, deserialized);
            }
        }

        #[test]
        fn array_use_deserialize() {
            for variant in ArrayUse::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(&(*dbg!(variant) as u8)))
                        .unwrap();
                let deserialized =
                    dbg!(serde_json::from_str::<'_, ArrayUse>(&serialized))
                        .unwrap();
                assert_eq!(*variant, deserialized);
            }

            for invalid in [0x11, 0xff, 0x7890] {
                let serialized = dbg!(serde_json::to_string(&invalid)).unwrap();

                dbg!(serde_json::from_str::<'_, ArrayUse>(&serialized))
                    .unwrap_err();
            }
        }

        #[test]
        fn error_correction_serde_roundtrip() {
            for variant in ErrorCorrection::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(dbg!(variant))).unwrap();
                let deserialized = dbg!(serde_json::from_str::<
                    '_,
                    ErrorCorrection,
                >(&serialized))
                .unwrap();
                assert_eq!(*variant, deserialized);
            }
        }

        #[test]
        fn error_correction_deserialize() {
            for variant in ErrorCorrection::VARIANTS {
                let serialized =
                    dbg!(serde_json::to_string(&(*dbg!(variant) as u8)))
                        .unwrap();
                let deserialized = dbg!(serde_json::from_str::<
                    '_,
                    ErrorCorrection,
                >(&serialized))
                .unwrap();
                assert_eq!(*variant, deserialized);
            }

            for invalid in [0x11, 0xff, 0x7890] {
                let serialized = dbg!(serde_json::to_string(&invalid)).unwrap();

                dbg!(serde_json::from_str::<'_, ErrorCorrection>(&serialized))
                    .unwrap_err();
            }
        }
    }
}

#[derive(Default)]
pub struct Type17 {
    pub phys_mem_array_handle: Handle,
    pub mem_err_info_handle: Handle,
    pub total_width: u16,
    pub data_width: u16,
    pub size: u16,
    pub form_factor: u8,
    pub device_set: u8,
    pub device_locator: SmbString,
    pub bank_locator: SmbString,
    pub memory_type: u8,
    pub type_detail: u16,
    pub speed: u16,
    pub manufacturer: SmbString,
    pub serial_number: SmbString,
    pub asset_tag: SmbString,
    pub part_number: SmbString,
    pub attributes: u8,
    pub extended_size: u32,
    pub cfgd_mem_clock_speed: u16,
    pub min_voltage: u16,
    pub max_voltage: u16,
    pub cfgd_voltage: u16,
}
impl Type17 {
    pub fn set_size(&mut self, size_bytes: Option<usize>) {
        match size_bytes {
            None => {
                self.size = 0xffff;
                self.extended_size = 0;
            }
            // size <= 32GiB - 1MiB does not need extended_size
            Some(n) if n < (32767 * MB) => {
                self.size = (n / MB) as u16;
            }
            Some(n) => {
                self.size = 0x7fff;
                self.extended_size = (n / MB) as u32;
            }
        }
    }
}
impl Table for Type17 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let mut stab = StringTable::new();
        let data = bits::Type17 {
            phys_mem_array_handle: self.phys_mem_array_handle.into(),
            mem_err_info_handle: self.mem_err_info_handle.into(),
            total_width: self.total_width,
            data_width: self.data_width,
            size: self.size,
            form_factor: self.form_factor,
            device_set: self.device_set,
            device_locator: stab.add(&self.device_locator),
            bank_locator: stab.add(&self.bank_locator),
            memory_type: self.memory_type,
            type_detail: self.type_detail,
            speed: self.speed,
            manufacturer: stab.add(&self.manufacturer),
            serial_number: stab.add(&self.serial_number),
            asset_tag: stab.add(&self.asset_tag),
            part_number: stab.add(&self.part_number),
            attributes: self.attributes,
            extended_size: self.extended_size,
            cfgd_mem_clock_speed: self.cfgd_mem_clock_speed,
            min_voltage: self.min_voltage,
            max_voltage: self.max_voltage,
            cfgd_voltage: self.cfgd_voltage,
            ..bits::Type17::new(handle.into())
        };

        render_table(data, None, Some(stab))
    }
}

#[derive(Default)]
pub struct Type32();
impl Table for Type32 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let data = bits::Type32::new(handle.into());

        // Boot status code for "no errors detected"
        let boot_status = [0u8];

        render_table(data, Some(&boot_status), None)
    }
}

#[derive(Default)]
pub struct Type127();
impl Table for Type127 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        bits::Type127::new(handle.into()).to_raw_bytes().into()
    }
}

/// Render all components of a SMBIOS table into raw bytes
///
/// # Arguments
/// - `raw_table`: [RawTable] instance representing the structure
/// - `extra_data`: Any data belonging in the formatted area of the structure
///   which is not already covered by its fields (variable length additions)
/// - `stab`: [StringTable] of any associated strings
fn render_table(
    mut raw_table: impl RawTable,
    extra_data: Option<&[u8]>,
    stab: Option<StringTable>,
) -> Vec<u8> {
    let extra_data = extra_data.unwrap_or(&[]);

    if extra_data.len() > 0 {
        let header = raw_table.header_mut();
        header.length = header
            .length
            .checked_add(extra_data.len() as u8)
            .expect("extra data does not overflow length");
    }
    let raw_data = raw_table.to_raw_bytes();

    // non-generic render, for when raw_table has been turned into bytes
    fn _render_table(
        raw_data: &[u8],
        extra_data: &[u8],
        stab: Option<StringTable>,
    ) -> Vec<u8> {
        let stab_data = stab.and_then(|stab| stab.render());

        let term_len = stab_data
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(bits::TABLE_TERMINATOR.len());

        let mut buf =
            Vec::with_capacity(raw_data.len() + extra_data.len() + term_len);
        buf.extend_from_slice(raw_data);
        buf.extend_from_slice(extra_data);
        if let Some(stab) = stab_data {
            buf.extend_from_slice(&stab);
        } else {
            buf.extend_from_slice(&bits::TABLE_TERMINATOR);
        }
        buf
    }

    _render_table(raw_data, extra_data, stab)
}

struct StringTable<'a> {
    strings: Vec<&'a SmbString>,
    len_with_nulls: usize,
}
impl<'a> StringTable<'a> {
    fn new() -> Self {
        Self { strings: Vec::new(), len_with_nulls: 0 }
    }
    /// Add a [SmbString] to the [StringTable], emitting its index value for
    /// inclusion in the structure to which it is being associated.
    fn add(&mut self, data: &'a SmbString) -> u8 {
        if data.is_empty() {
            0u8
        } else {
            assert!(self.strings.len() < 254);
            self.len_with_nulls += data.len() + 1;
            self.strings.push(data);
            let idx = self.strings.len() as u8;

            idx
        }
    }
    /// Render associated strings raw bytes, properly formatted to be appended
    /// to an associated SMBIOS table.  Returns `None` if no strings were added
    /// to the table.
    fn render(mut self) -> Option<Vec<u8>> {
        if self.strings.is_empty() {
            None
        } else {
            let mut out = Vec::with_capacity(self.len_with_nulls + 1);
            for string in self.strings.drain(..) {
                out.extend_from_slice(string.as_ref());
                out.push(b'\0');
            }
            // table expected to end with double-NUL
            out.push(b'\0');
            Some(out)
        }
    }
}
