// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instance specifications: abstract descriptions of a VM's devices and config.
//!
//! An instance spec describes a VM's virtual devices, backends, and other
//! guest environment configuration supplied by the Propolis VMM. RFD 283
//! contains more details about how specs are used throughout the Oxide stack
//! and about the versioning considerations described below.
//!
//! # Module layout
//!
//! The data types in this module are taxonomized into "components" and
//! versioned "spec structures." Components are the "leaves" of a spec; each
//! component specifies an individual component or piece of functionality that
//! can be added to a VM. The strongly versioned structure types arrange these
//! components in a specific way; each organization is a version of the overall
//! instance spec structure.
//!
//! # Versioning & compatibility
//!
//! Instance specs may be sent between Propolises, sled agents, and Nexus
//! processes that use different versions of this library, so the library needs
//! to provide a versioning scheme that allows specs to be extended over time.
//! Such scheme must balance safety against developer toil. Strongly versioning
//! data types--requiring a new API endpoint or type definition every time
//! something changes--minimizes the risk that a data structure will be
//! misinterpreted, but is very toilsome to maintain, since changing one
//! structure may require many other structures to be revised and
//! `From`/`TryFrom` impls to be added for all the new version combinations.
//! Weaker versioning schemes require less toil to maintain but run the risk
//! that a spec user will be too permissive and will misconfigure a VM because
//! it missed some important context in a
//! spec that it was passed.
//!
//! This module balances these concerns as follows:
//!
//! - **Components** are versionless but are allowed to be extended in backward-
//!   compatible ways (i.e., such that a spec produced by an old library can be
//!   interpreted correctly by a newer library). Breaking changes to components
//!   are not allowed and require a new component to be defined.
//! - **Spec structures** are strongly versioned. Backward-compatible changes to
//!   an existing version are technically allowed, but completely restructuring
//!   a spec requires a new spec version and a corresponding variant in the
//!   `VersionedInstanceSpec` structure.
//!
//! This scheme assumes that (a) components are likely to be added or changed
//! much more frequently than the spec structure itself will be revised, and (b)
//! most changes to existing components can easily be made backward-compatible
//! (e.g. by wrapping new functionality in an `Option` and taking a `None` value
//! to mean "do what all previous versions did").
//!
//! ## Compatibility rules & breaking changes
//!
//! Changes to existing data types must be backward compatible with older spec
//! versions: a spec produced by an old version of the library must always be
//! deserializable by a new version of the library.
//!
//! The following component changes are not backward compatible:
//!
//! - Adding a new required field to a struct or enum variant
//! - Removing a field from a struct or enum variant
//! - Renaming structs, enums, or their fields or variants
//!
//! Adding new *optional* fields to a struct or enum variant is OK provided that
//! the default value's semantics match the semantics expected by users of older
//! specs that don't provide the optional data.
//!
//! Forward compatibility--writing the library so that old versions can
//! interpret specs generated by new versions--is not generally guaranteed.
//! Where possible, however, spec components should be written so that it is
//! possible to downgrade from a newer spec version to an older one if a
//! component's configuration can be represented in both versions.
//!
//! ## Serde attributes
//!
//! This module doesn't directly verify that a specific Propolis version can
//! support all of the features in any particular specification. However, users
//! can generally expect that if Propolis is willing to deserialize a spec, then
//! it should be able (in at least some circumstances) to support all of the
//! features that can be expressed in that spec. To help guarantee this property
//! (i.e., if Propolis can deserialize it, then it's at least well-formed), this
//! module uses a few common `serde` attributes.
//!
//! Structs and enums in this module should be tagged with the
//! `#[serde(deny_unknown_fields)]` attribute to reduce the risk that old code
//! will silently drop information from a spec produced by newer code with more
//! available fields.
//!
//! New optional fields should use the `#[serde(default)]` field attribute to
//! provide backward compatibility to old specs. They can also use the
//! `#[serde(skip_serializing_if)]` attribute to avoid serializing new fields
//! that have their default values.
//!
//! ### Example
//!
//! As an example, consider a (hypothetical) virtio device that has backend name
//! and PCI path fields:
//!
//! ```
//! # use serde::{Serialize, Deserialize};
//! # use schemars::JsonSchema;
//! # use propolis_types::PciPath;
//!
//! #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
//! #[serde(deny_unknown_fields)]
//! struct VirtioComponent {
//!     backend_name: String,
//!     pci_path: PciPath
//! }
//! ```
//!
//! Suppose Propolis then adds support for configuring the number of virtqueues
//! this device exposes to the guest. This can be expressed compatibly as
//! follows:
//!
//! ```
//! # use serde::{Serialize, Deserialize};
//! # use schemars::JsonSchema;
//! # use propolis_types::PciPath;
//!
//! #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
//! #[serde(deny_unknown_fields)]
//! struct VirtioComponent {
//!     backend_name: String,
//!     pci_path: PciPath,
//!
//!     #[serde(default, skip_serializing_if = "Option::is_none")]
//!     num_virtqueues: Option<usize>
//! }
//! ```
//!
//! Old component specs will continue to deserialize with `num_virtqueues` set
//! to `None`. In this case Propolis ensures that the device gets the default
//! number of virtqueues it had before this configuration option was added. If
//! this spec is serialized again, the `num_virtqueues` option is omitted, so
//! the spec can be deserialized by downlevel versions of the library. Note
//! again that the former behavior (new library accepts old spec) is required,
//! while the latter behavior (old library accepts new spec) is nice to have and
//! may not always be possible to provide (e.g. if the value is `Some`).
//!
//! ## Naming of versioned structures
//!
//! Dropshot's OpenAPI schema generator has a known limitation. If a type or one
//! of its dependent types appears in an API, Dropshot adds to the API's schema
//! an object type with the type's name. If two separate types with the same
//! name but *different module paths* appear in the API, Dropshot chooses one
//! to include and silently ignores the rest. This issue is
//! [dropshot#383](https://github.com/oxidecomputer/dropshot/issues/383). To
//! avoid it, strongly versioned types in this module use a "V#" suffix in their
//! names, even though they may reside in separate versioned modules.

#![allow(rustdoc::private_intra_doc_links)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use propolis_types::{CpuidIdent, CpuidValues, CpuidVendor, PciPath};

pub mod components;
pub mod v0;

/// Type alias for keys in the instance spec's maps.
type SpecKey = String;

/// A versioned instance spec.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, tag = "version", content = "spec")]
pub enum VersionedInstanceSpec {
    V0(v0::InstanceSpecV0),
}
