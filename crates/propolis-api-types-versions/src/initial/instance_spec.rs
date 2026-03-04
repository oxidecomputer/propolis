// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instance specification types for the INITIAL API version.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use propolis_types::{CpuidIdent, CpuidValues, CpuidVendor, PciPath};

use super::components::{backends, board, devices};

/// A key identifying a component in an instance spec.
//
// Some of the components Omicron attaches to Propolis VMs, like network
// interfaces and Crucible disks, are described by database records with UUID
// primary keys. It's natural to reuse these UUIDs as component identifiers in
// Propolis, especially because it lets Omicron functions that need to identify
// a specific component (e.g. a specific Crucible backend that should handle a
// disk snapshot request) pass that component's ID directly to Propolis.
//
// In some cases it's not desirable or possible to use UUIDs this way:
//
// - Some components (like the cloud-init disk) don't have their own rows in the
//   database and so don't have obvious UUIDs to use.
// - Some objects (like Crucible disks) require both a device and a backend
//   component in the spec, and these can't share the same key.
// - Propolis users outside the control plane may not have any component UUIDs
//   at all and may just want to use strings to identify all their components.
//
// For these reasons, the key type may be represented as either a UUID or a
// String. This allows the more compact, more-easily-compared UUID format to be
// used wherever it is practical while still allowing callers to use strings as
// names if they have no UUIDs available or the most obvious UUID is in use
// elsewhere. The key type's From impls will try to parse strings into UUIDs
// before storing keys as strings.
#[derive(
    Clone, Debug, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd,
)]
// Direct serde to use an untagged enum representation for this type. Since both
// Uuid and String serialize to strings, this allows other types that contain a
// Map<K = SpecKey> to derive Serialize and successfully serialize to JSON.
// (This doesn't work with a tagged representation because JSON doesn't allow
// maps to be used as map keys.)
//
// Note that this makes the order of variants matter: serde will pick the first
// variant into which it can successfully deserialize an untagged enum value,
// and the point is to use the UUID representation for any value that can be
// interpreted as a UUID.
#[serde(untagged)]
pub enum SpecKey {
    Uuid(Uuid),
    Name(String),
}

// Manually implement JsonSchema to help Progenitor generate the expected enum
// type for spec keys.
impl JsonSchema for SpecKey {
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
                    label_schema("uuid", generator.subschema_for::<Uuid>()),
                    label_schema("name", generator.subschema_for::<String>()),
                ]),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    content = "component",
    rename_all = "snake_case"
)]
#[schemars(rename = "ComponentV0")]
pub enum Component {
    VirtioDisk(devices::VirtioDisk),
    NvmeDisk(devices::NvmeDisk),
    VirtioNic(devices::VirtioNic),
    SerialPort(devices::SerialPort),
    PciPciBridge(devices::PciPciBridge),
    QemuPvpanic(devices::QemuPvpanic),
    BootSettings(devices::BootSettings),
    SoftNpuPciPort(devices::SoftNpuPciPort),
    SoftNpuPort(devices::SoftNpuPort),
    SoftNpuP9(devices::SoftNpuP9),
    P9fs(devices::P9fs),
    MigrationFailureInjector(devices::MigrationFailureInjector),
    CrucibleStorageBackend(backends::CrucibleStorageBackend),
    FileStorageBackend(backends::FileStorageBackend),
    BlobStorageBackend(backends::BlobStorageBackend),
    VirtioNetworkBackend(backends::VirtioNetworkBackend),
    DlpiNetworkBackend(backends::DlpiNetworkBackend),
}

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceSpec {
    pub board: board::Board,
    pub components: BTreeMap<SpecKey, Component>,
}

/// DEPRECATED: A versioned instance spec.
///
/// This structure is deprecated. It is notionally incompatible with
/// dropshot API versioning. If you wanted to add a `V1` variant to
/// `VersionedInstanceSpec`, it would change the existing blessed V1 OpenAPI
/// spec. Therefore you'd have to rename this to `VersionedInstanceSpecV0` and
/// create a new type with the new variant. This makes little sense however,
/// and is a remnant of an attempt at propolis versioning prior to dropshot
/// API versioning.
///
/// Luckily this type is only exposed via `InstanceSpecGetResponse` which is not
/// used in Omicron. Therefore we can limit that method to the V1 OpenAPI spec
/// and stop any further use of this type.
///
/// In addition to the disparate versioning mechanisms, there is also a
/// fundamental flaw in how this type was used in the existing code. It was
/// constructed in some cases from a `MaybeSpec` which contains a `Box<Spec>`.
/// Unfortunately, `Spec` is a type erased container of any version of an
/// instance spec such as `InstanceSpecV0`,`InstanceSpecV1`, or future types.
/// There is no guarantee that we could take a `Spec` and figure out which
/// versioned spec it is supposed to convert to. This only worked in the initial
/// code because the only versioned spec was `InstanceSpecV0`. It's best to stop
/// using this type altogether.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, tag = "version", content = "spec")]
pub enum VersionedInstanceSpec {
    V0(InstanceSpec),
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", content = "value")]
pub enum InstanceSpecStatus {
    WaitingForMigrationSource,
    Present(VersionedInstanceSpec),
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceSpecGetResponse {
    pub properties: super::instance::InstanceProperties,
    pub state: super::instance::InstanceState,
    pub spec: InstanceSpecStatus,
}
