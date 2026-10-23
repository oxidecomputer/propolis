// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VM mainboard components. Every VM has a board, even if it has no other
//! peripherals.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1::components::board::{Chipset, Cpuid, GuestHypervisorInterface};
use crate::v1::instance_spec::CpuidVendor;

/// A collection of CPUID leaves to specialize for a board's `CpuTopology`.
///
/// Leaves may be selected here without being present in `Cpuid`, in which case
/// the extra selected topology leaves are ignored. Conversely, `Cpuid` leaves
/// with topology information that are not mentioend here probably indicate a
/// bug (at best, those leaves will probably indicate collection of single-core
/// sockets in the machine.)
#[derive(Copy, Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TopoLeaves {
    pub std_4: bool,
    pub std_b: bool,
    pub ext_8: bool,
    pub ext_1e: bool,
}

/// A *very* simple topology for for a VM's vCPUs.
///
/// Typically this will be configured as `threads_per_core: 2` to indicate SMT,
/// and one socket. The number of vCPUs in each socket is the same, meaning that
/// the number of vCPUs must divide evenly into the number of sockets in the
/// topology.
// TODO: Should this express a NUMA topology as well? Probably! Though an
// explicit topology would make migration trickier..
// TODO: How does this describe CPU bindings? Should it? Probably. Nexus will
// eventually make decisions about what VM's vCPUs are placed where on sleds,
// and we'll want to faithfully indicate information about what processors are
// physically adjacent .... *probably*; doing that it harder to migrate without
// starting to lie.
#[derive(Copy, Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CpuTopology {
    pub has_smt: bool,
    pub sockets: u16,
    // One might imagine TopoLeaves on either `Cpuid` or here, with topology information. I settled
    // on Board->CpuTopology->TopoLeaves because CpuTopology ultimately decides aspects like "how
    // many sockets does the guest think this board has", so the topology leaves we specialize are
    // more "part of the board" than "part of the CPUs going in the board". Since a reasonable
    // interpretation of CPUID leaves are that they come "from" a CPU in a socket, it is a little
    // funky that these aren't part of `Cpuid`.
    //
    // Metaphysically speaking, there are kind of `MAX_CPUS * permutations(TopoLeaves)` distinct
    // "CPU SKUs" that could be present, so what's really important is that the specialization
    // `(Cpuid, vcpus, CpuTopology) -> CpuidSet` is stable.
    pub topo_leaves: TopoLeaves,
}

/// A VM's mainboard.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Board {
    /// The number of virtual logical processors attached to this VM.
    pub cpus: u16,

    /// The amount of guest RAM attached to this VM.
    pub memory_mb: u64,

    /// The chipset to expose to guest software.
    pub chipset: Chipset,

    /// The hypervisor platform to expose to the guest. The default is a
    /// bhyve-compatible interface with no additional features.
    ///
    /// For compatibility with older versions of Propolis, this field is only
    /// serialized if it specifies a non-default interface.
    #[serde(
        default,
        skip_serializing_if = "GuestHypervisorInterface::is_default"
    )]
    pub guest_hv_interface: GuestHypervisorInterface,

    /// The CPUID values to expose to the guest. If `None`, bhyve will derive
    /// default values from the host's CPUID values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpuid: Option<Cpuid>,

    /// A *very* simplistic description of this VM's CPU topology.
    pub cpu_topology: CpuTopology,
}
