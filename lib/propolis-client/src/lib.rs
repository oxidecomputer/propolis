// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A client for the Propolis hypervisor frontend's server API.

// Re-export the PCI path type from propolis_api_types so that users can access
// its constructor and From impls.
pub use propolis_api_types::instance_spec::PciPath;

// Re-export Crucible client types that appear in their serialized forms in
// instance specs. This allows clients to ensure they serialize/deserialize
// these types using the same versions as the Propolis client associated with
// the server they want to talk to.
pub use crucible_client_types::{CrucibleOpts, VolumeConstructionRequest};

progenitor::generate_api!(
    spec = "../../openapi/propolis-server.json",
    interface = Builder,
    tags = Separate,
    replace = {
        PciPath = crate::PciPath,
    },
    patch = {
        BootOrderEntry = { derives = [schemars::JsonSchema] },
        BootSettings = { derives = [Default, schemars::JsonSchema] },
        CpuidEntry = { derives = [PartialEq, Eq, Copy] },
        InstanceMetadata = { derives = [ PartialEq ] },
    }
);

pub mod support;
