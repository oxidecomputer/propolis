// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A client for the Propolis hypervisor frontend's server API.

/// Re-exports of types related to instance specs.
///
/// These types are re-exported for the convenience of components like
/// sled-agent that may wish to expose instance specs in their own APIs.
/// Defining the sled-agent API in terms of these "native" types allows
/// sled-agent to reuse their trait implementations (and in particular use
/// "manual" impls of things that Progenitor would otherwise derive).
///
/// In the generated client, the native "top-level" instance spec and component
/// types ([`crate::instance_spec::VersionedInstanceSpec`],
/// [`crate::instance_spec::InstanceSpecV0`], and
/// [`crate::instance_spec::ReplacementComponent`]) replace their generated
/// counterparts. This obviates the need to maintain `From` impls to convert
/// between native and generated types.
pub mod instance_spec {
    pub use propolis_api_types::instance_spec::{
        components::{backends::*, board::*, devices::*},
        v0::*,
        *,
    };

    pub use propolis_api_types::{
        InstanceMetadata, InstanceProperties, InstanceSpec,
        InstanceSpecGetResponse, InstanceSpecStatus, ReplacementComponent,
    };
}

// Re-export Crucible client types that appear in their serialized forms in
// instance specs. This allows clients to ensure they serialize/deserialize
// these types using the same versions as the Propolis client associated with
// the server they want to talk to.
pub use crucible_client_types::{CrucibleOpts, VolumeConstructionRequest};

progenitor::generate_api!(
    spec = "../../openapi/propolis-server/propolis-server-latest.json",
    interface = Builder,
    tags = Separate,
    replace = {
        PciPath = crate::instance_spec::PciPath,
        ReplacementComponent = crate::instance_spec::ReplacementComponent,
        InstanceSpecV0 = crate::instance_spec::InstanceSpecV0,
        InstanceSpec = crate::instance_spec::InstanceSpec,
        InstanceSpecStatus = crate::instance_spec::InstanceSpecStatus,
        InstanceProperties = crate::instance_spec::InstanceProperties,
        InstanceMetadata = crate::instance_spec::InstanceMetadata,
        InstanceSpecGetResponse = crate::instance_spec::InstanceSpecGetResponse,
        VersionedInstanceSpec = crate::instance_spec::VersionedInstanceSpec,
        CpuidEntry = crate::instance_spec::CpuidEntry,
    },
    // Automatically derive JsonSchema for instance spec-related types so that
    // they can be reused in sled-agent's API. This can't be done with a
    // `derives = [schemars::JsonSchema]` directive because the `SpecKey` type
    // needs to implement that trait manually (see below).
    patch = {
        BootSettings = { derives = [ Default ] },
        CpuidEntry = { derives = [ PartialEq, Eq, Copy ] },
        InstanceMetadata = { derives = [ PartialEq ] },
        SpecKey = { derives = [ PartialEq, Eq, Ord, PartialOrd, Hash ] },
    }
);

pub mod support;
