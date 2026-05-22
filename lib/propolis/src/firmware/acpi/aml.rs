// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Collection of AML helpers and wrappers.

use acpi_tables::aml;

/// Creates an IO port with a fixed port number.
///
/// The AML IO operation takes a min and max range of acceptable port numbers.
/// To create a fixed IO port allocation, min and max must be set to the same
/// value, which can look confusing.
///
/// ACPI rev. 6.6 section 6.4.2.5 "I/O Port Descriptor"
pub fn io_port(port: u16, alignment: u8, length: u8) -> aml::IO {
    aml::IO::new(port, port, alignment, length)
}

/// Constructors for ACPI paths defined in the ACPI specification.
///
/// ACPI rev. 6.6 section 5.3 "ACPI Namespace" describes the (limited) syntax
/// for names; you may want to read before adding or editing items in this
/// module.
pub mod paths {
    use acpi_tables::aml;

    macro_rules! path {
        ($fn:ident, $name:expr) => {
            pub fn $fn() -> aml::Path {
                aml::Path::new($name)
            }
        };
    }

    // Object that evaluates to a device's address on its parent bus.
    //
    // ACPI rev. 6.6 section 6.1.1 "_ADR (Address)"
    path!(adr, "_ADR");

    // PCI bus number set up by the platform boot firmware.
    //
    // ACPI rev. 6.6 section 6.5.5 "_BBN (Base Bus Number)"
    path!(bbn, "_BBN");

    // Object that evaluates to a device's Plug and Play-compatible ID list.
    //
    // ACPI rev. 6.6 section 6.1.2 "_CID (Compatible ID)"
    path!(cid, "_CID");

    // Object that specifies a device's current resource settings, or a control
    // method that generates such an object.
    //
    // ACPI rev. 6.6 section 6.2.2 "_CRS (Current Resource Settings)"
    path!(crs, "_CRS");

    // Object that associates a logical software name (for example, COM1) with
    // a device.
    //
    // ACPI rev. 6.6 section 6.1.4 "_DDN (DOS Device Name)"
    path!(ddn, "_DDN");

    // Control method that disables a device.
    //
    // ACPI rev. 6.6 section 6.2.3 "_DIS (Disable)"
    path!(dis, "_DIS");

    // Object that evaluates to a device's Plug and Play hardware ID.
    //
    // ACPI rev. 6.6 section 6.1.5 "_HID (Hardware ID)"
    path!(hid, "_HID");

    // An object that specifies a device's possible resource settings, or a
    // control method that generates such an object.
    //
    // ACPI rev. 6.6 section 6.2.13 "_PRS (Possible Resource Settings)"
    path!(prs, "_PRS");

    // Object that specifies the PCI interrupt routing table.
    //
    // ACPI rev. 6.6 section 6.2.14 "_PRT (PCI Routing Table)"
    path!(prt, "_PRT");

    // Control method that sets a device's settings.
    //
    // ACPI rev. 6.6 section 6.2.17 "_SRS (Set Resource Settings)"
    path!(srs, "_SRS");

    // Control method that returns a device's status.
    //
    // ACPI rev. 6.6 section 6.3.7 "_STA (Device Status)"
    path!(sta, "_STA");

    // Object that specifies a device's unique persistent ID, or a control
    // method that generates it.
    //
    // ACPI rev. 6.6 section 6.1.12 "_UID (Unique ID)"
    path!(uid, "_UID");
}

/// Constructors for ACPI names defined in the ACPI specification.
///
/// Refer to [paths] for more information.
pub mod names {
    use super::paths;
    use acpi_tables::{aml, Aml};

    macro_rules! name {
        ($fn:ident) => {
            pub fn $fn(inner: &dyn Aml) -> aml::Name {
                aml::Name::new(paths::$fn(), inner)
            }
        };
    }

    name!(adr);
    name!(bbn);
    name!(cid);
    name!(crs);
    name!(ddn);
    name!(hid);
    name!(prs);
    name!(sta);
    name!(uid);
}

/// Constructors for ACPI methods defined in the ACPI specification.
///
/// Refer to [paths] for more information.
pub mod methods {
    use super::paths;
    use acpi_tables::{aml, Aml};

    macro_rules! method {
        ($fn:ident) => {
            pub fn $fn<'a>(
                args: u8,
                serialized: bool,
                children: Vec<&'a dyn Aml>,
            ) -> aml::Method<'a> {
                aml::Method::new(paths::$fn(), args, serialized, children)
            }
        };
    }

    method!(crs);
    method!(dis);
    method!(prs);
    method!(prt);
    method!(srs);
    method!(sta);
}

/// Device ID and Plug and Play (`PNP`) device codes used throughout ACPI tables.
/// UEFI and ACPI use standardized IDs as described in https://uefi.org/PNP_ACPI_Registry,
/// which itself points to reserved device IDs at
/// https://uefi.org/sites/default/files/resources/devids%20%285%29.txt
pub mod devids {
    // --Interrupt Controllers--
    pub const AT_INT_CONTROLLER: &'static str = "PNP0000";

    // --Timers--
    pub const AT_TIMER: &'static str = "PNP0100";

    // --DMA--
    pub const AT_DMA_CONTROLLER: &'static str = "PNP0200";

    // --Keyboards--
    pub const IBM_ENHANCED_KEYBOARD: &'static str = "PNP0303";
    pub const MICROSOFT_RESERVED_KEYBOARD: &'static str = "PNP030B";

    // --Serial Devices--
    pub const COM_PORT_16550A: &'static str = "PNP0501";

    // --Peripheral Buses--
    pub const PCI_BUS: &'static str = "PNP0A03";

    // --Real Time Clock, BIOS, System board devices--
    pub const AT_SPEAKER_SOUND: &'static str = "PNP0800";
    pub const AT_REAL_TIME_CLOCK: &'static str = "PNP0B00";
    pub const GENERAL_ID: &'static str = "PNP0C02";
    pub const MATH_COPROCESSOR: &'static str = "PNP0C04";
    pub const PCI_INT_LINK: &'static str = "PNP0C0F";

    // --QEMU---
    // https://www.qemu.org/docs/master/specs/pvpanic.html
    pub const QEMU_PVPANIC: &'static str = "QEMU0001";
}
