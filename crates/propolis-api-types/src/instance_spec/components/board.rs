// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VM mainboard components. Every VM has a board, even if it has no other
//! peripherals.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::instance_spec::CpuidVendor;

/// An Intel 440FX-compatible chipset.
#[derive(
    Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct I440Fx {
    /// Specifies whether the chipset should allow PCI configuration space
    /// to be accessed through the PCIe extended configuration mechanism.
    pub enable_pcie: bool,
}

/// A kind of virtual chipset.
#[derive(
    Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema,
)]
#[serde(
    deny_unknown_fields,
    rename_all = "snake_case",
    tag = "type",
    content = "value"
)]
pub enum Chipset {
    /// An Intel 440FX-compatible chipset.
    I440Fx(I440Fx),
}

/// A set of CPUID values to expose to a guest.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cpuid {
    /// A list of CPUID leaves/subleaves and their associated values.
    ///
    /// The `leaf` and `subleaf` fields of each entry in the template must
    /// be unique. The leaf must also be in the standard or extended ranges
    /// (0 to 0xFFFF or 0x8000_0000 to 0x8000_FFFF) defined in the `cpuid_utils`
    /// crate.
    //
    // It would be nice if this were an associative collection type.
    // Unfortunately, the most natural keys for such a collection are
    // structs or tuples, and JSON doesn't allow objects to be used as
    // property names. Instead of converting leaf/subleaf pairs to and from
    // strings, just accept a flat Vec and have servers verify that e.g. no
    // leaf/subleaf pairs are duplicated.
    pub entries: Vec<CpuidEntry>,

    /// The CPU vendor to emulate.
    ///
    /// CPUID leaves in the extended range (0x8000_0000 to 0x8000_FFFF) have
    /// vendor-defined semantics. Propolis uses this value to determine
    /// these semantics when deciding whether it needs to specialize the
    /// supplied template values for these leaves.
    pub vendor: CpuidVendor,
}

/// A full description of a CPUID leaf/subleaf and the values it produces.
#[derive(
    Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct CpuidEntry {
    /// The leaf (function) number for this entry.
    pub leaf: u32,

    /// The subleaf (index) number for this entry, if it uses subleaves.
    pub subleaf: Option<u32>,

    /// The value to return in eax.
    pub eax: u32,

    /// The value to return in ebx.
    pub ebx: u32,

    /// The value to return in ecx.
    pub ecx: u32,

    /// The value to return in edx.
    pub edx: u32,
}

/// A VM's mainboard.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Board {
    /// The number of virtual logical processors attached to this VM.
    pub cpus: u8,

    /// The amount of guest RAM attached to this VM.
    pub memory_mb: u64,

    /// The chipset to expose to guest software.
    pub chipset: Chipset,

    /// The CPUID values to expose to the guest. If `None`, bhyve will derive
    /// default values from the host's CPUID values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpuid: Option<Cpuid>,
    // TODO: Processor and  topology.
}
