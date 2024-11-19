// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A client for the Propolis hypervisor frontend's server API.

// Re-export types from propolis_api_types where callers may want to use
// constructors or From impls.
pub use propolis_api_types::instance_spec::{PciPath, SpecKey};

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
        SpecKey = crate::SpecKey,
    },
    patch = {
        BootOrderEntry = { derives = [schemars::JsonSchema] },
        BootSettings = { derives = [Default, schemars::JsonSchema] },
        CpuidEntry = { derives = [PartialEq, Eq, Copy] },
        InstanceMetadata = { derives = [ PartialEq ] },
    }
);

pub mod support;
