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
    pub use propolis_api_types_versions::latest::components::{
        backends::*, board::*, devices::*,
    };
    pub use propolis_api_types_versions::latest::instance::{
        InstanceMetadata, InstanceProperties, ReplacementComponent,
    };
    pub use propolis_api_types_versions::latest::instance_spec::*;
    // Re-export v1 types with V0 suffix for backward compatibility with
    // progenitor-generated clients.
    pub use propolis_api_types_versions::v1::instance_spec::{
        Component as ComponentV0, InstanceSpec as InstanceSpecV0,
        InstanceSpecGetResponse as InstanceSpecGetResponseV0,
        InstanceSpecStatus as InstanceSpecStatusV0, VersionedInstanceSpec,
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
        PciPath = propolis_api_types_versions::latest::instance_spec::PciPath,
        ReplacementComponent = propolis_api_types_versions::latest::instance::ReplacementComponent,
        InstanceSpec = propolis_api_types_versions::latest::instance_spec::InstanceSpec,
        InstanceSpecStatus = propolis_api_types_versions::latest::instance_spec::InstanceSpecStatus,
        InstanceProperties = propolis_api_types_versions::latest::instance::InstanceProperties,
        InstanceMetadata = propolis_api_types_versions::latest::instance::InstanceMetadata,
        InstanceSpecGetResponse = propolis_api_types_versions::latest::instance_spec::InstanceSpecGetResponse,
        NvmeDisk = propolis_api_types_versions::latest::components::devices::NvmeDisk,
        SmbiosType1Input = propolis_api_types_versions::latest::instance_spec::SmbiosType1Input,
        VersionedInstanceSpec = propolis_api_types_versions::latest::instance_spec::VersionedInstanceSpec,
        CpuidEntry = propolis_api_types_versions::latest::components::board::CpuidEntry,
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
