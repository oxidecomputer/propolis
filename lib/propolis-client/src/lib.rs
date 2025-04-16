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
    },
    // Automatically derive JsonSchema for instance spec-related types so that
    // they can be reused in sled-agent's API. This can't be done with a
    // `derives = [schemars::JsonSchema]` directive because the `SpecKey` type
    // needs to implement that trait manually (see below).
    patch = {
        BlobStorageBackend = { derives = [ schemars::JsonSchema ]},
        Board = { derives = [ schemars::JsonSchema ]},
        BootOrderEntry = { derives = [ schemars::JsonSchema ]},
        BootSettings = { derives = [ schemars::JsonSchema, Default] },
        ComponentV0 = { derives = [ schemars::JsonSchema ]},
        Chipset = { derives = [ schemars::JsonSchema ]},
        CrucibleStorageBackend = { derives = [ schemars::JsonSchema ]},
        Cpuid = { derives = [ schemars::JsonSchema ]},
        CpuidEntry = { derives = [ schemars::JsonSchema, PartialEq, Eq, Copy ]},
        CpuidVendor = { derives = [ schemars::JsonSchema ]},
        DlpiNetworkBackend = { derives = [ schemars::JsonSchema ]},
        FileStorageBackend = { derives = [ schemars::JsonSchema ]},
        GuestHypervisorInterface = { derives = [ schemars::JsonSchema ]},
        I440Fx = { derives = [ schemars::JsonSchema ]},
        HyperVFeatureFlag = { derives = [ schemars::JsonSchema ]},
        MigrationFailureInjector = { derives = [ schemars::JsonSchema ]},
        NvmeDisk = { derives = [ schemars::JsonSchema ]},
        InstanceMetadata = { derives = [ PartialEq ] },
        InstanceSpecV0 = { derives = [ schemars::JsonSchema ]},
        PciPciBridge = { derives = [ schemars::JsonSchema ]},
        P9fs = { derives = [ schemars::JsonSchema ]},
        QemuPvpanic = { derives = [ schemars::JsonSchema ]},
        SerialPort = { derives = [ schemars::JsonSchema ]},
        SerialPortNumber = { derives = [ schemars::JsonSchema ]},
        SoftNpuP9 = { derives = [ schemars::JsonSchema ]},
        SoftNpuPort = { derives = [ schemars::JsonSchema ]},
        SoftNpuPciPort = { derives = [ schemars::JsonSchema ]},
        SpecKey = { derives = [ PartialEq, Eq, Ord, PartialOrd, Hash ] },
        VirtioDisk = { derives = [ schemars::JsonSchema ]},
        VirtioNic = { derives = [ schemars::JsonSchema ]},
        VirtioNetworkBackend = { derives = [ schemars::JsonSchema ]},
    }
);

// Supply the same JsonSchema implementation for the generated SpecKey type that
// the native type has. This allows sled-agent (or another consumer) to reuse
// the generated type in its own API to produce an API document that generates
// the correct type for sled-agent's (or the other consumer's) clients.
impl schemars::JsonSchema for types::SpecKey {
    fn schema_name() -> String {
        "SpecKey".to_owned()
    }

    fn json_schema(
        generator: &mut schemars::gen::SchemaGenerator,
    ) -> schemars::schema::Schema {
        use schemars::schema::*;
        fn label_schema(label: &str, schema: Schema) -> Schema {
            SchemaObject {
                metadata: Some(
                    Metadata {
                        title: Some(label.to_string()),
                        ..Default::default()
                    }
                    .into(),
                ),
                subschemas: Some(
                    SubschemaValidation {
                        all_of: Some(vec![schema]),
                        ..Default::default()
                    }
                    .into(),
                ),
                ..Default::default()
            }
            .into()
        }

        SchemaObject {
            metadata: Some(
                Metadata {
                    description: Some(
                        "A key identifying a component in an instance spec."
                            .to_string(),
                    ),
                    ..Default::default()
                }
                .into(),
            ),
            subschemas: Some(Box::new(SubschemaValidation {
                one_of: Some(vec![
                    label_schema(
                        "uuid",
                        generator.subschema_for::<uuid::Uuid>(),
                    ),
                    label_schema("name", generator.subschema_for::<String>()),
                ]),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

pub mod support;
