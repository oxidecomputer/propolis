// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI table and AML bytecode generation.

pub mod tables;

pub use tables::{Dsdt, Fadt, Hpet, Madt, Mcfg, Rsdt, Rsdp, Xsdt};
