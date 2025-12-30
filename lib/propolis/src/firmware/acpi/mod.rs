// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI table and AML bytecode generation.

pub mod aml;
pub mod dsdt;
pub mod names;
pub mod opcodes;
pub mod resources;
pub mod tables;

pub use aml::{AmlBuilder, AmlWriter, DeviceGuard, MethodGuard, ScopeGuard};
pub use dsdt::{build_dsdt_aml, DsdtConfig, DsdtGenerator, DsdtScope, PcieConfig};
pub use names::EisaId;
pub use resources::ResourceTemplateBuilder;
pub use tables::{Dsdt, Facs, Fadt, Hpet, Madt, Mcfg, Rsdt, Rsdp, Xsdt};
