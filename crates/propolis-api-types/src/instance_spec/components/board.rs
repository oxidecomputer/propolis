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
pub enum Cpuid {
    /// Allow the host to supply whatever CPUID values it exposes to guests by
    /// default. This is useful for ad hoc testing where a user may not want to
    /// provide an explicit set of CPUID values.
    HostDefault,

    /// Use the specified CPUID entries as a template to produce the CPUID
    /// values that will be programmed into the instance's vCPUs.
    ///
    /// Propolis and/or bhyve may further modify the values supplied here before
    /// exposing them to the guest. For example, some leaves return data that
    /// depends on the APIC ID of the processor that executed CPUID, and some
    /// leaves' return values depend on the values of other system registers
    /// (e.g. CR4) that the guest can write at runtime.
    ///
    /// During live migration, sources and targets use identical CPUID
    /// templates. Any CPUID values that depend on
    Template {
        /// A list of CPUID leaves/subleaves and their associated values.
        ///
        /// The `leaf` and `subleaf` fields of each entry in the template must
        /// be unique. The leaf must also be in the standard or extended ranges
        /// (0 to 0xFFFF or 0x8000_0000 to 0x8000_FFFF).
        //
        // It would be nice if this were an associative collection type.
        // Unfortunately, the most natural keys for such a collection are
        // structs or tuples, and JSON doesn't allow objects to be used as
        // property names. Instead of converting leaf/subleaf pairs to and from
        // strings, just accept a flat Vec and have servers verify that e.g. no
        // leaf/subleaf pairs are duplicated.
        entries: Vec<CpuidEntry>,

        /// The CPU vendor to emulate.
        ///
        /// CPUID leaves in the extended range (0x8000_0000 to 0x8000_FFFF) have
        /// vendor-defined semantics. Propolis uses this value to determine
        /// these semantics when deciding whether it needs to specialize the
        /// supplied template values for these leaves.
        vendor: CpuidVendor,
    },
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

    /// The CPUID values to expose to the guest.
    pub cpuid: Cpuid,
    // TODO: Processor and memory topology.
}

impl Default for Board {
    fn default() -> Self {
        Self {
            cpus: 0,
            memory_mb: 0,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
            cpuid: Cpuid::HostDefault,
        }
    }
}
