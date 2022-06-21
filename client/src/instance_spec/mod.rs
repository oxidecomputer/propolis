//! Instance specifications: abstract descriptions of a VM's devices and config.
//!
//! An instance spec describes a VM's virtual devices, backends, and other
//! guest environment configuration supplied by the Propolis VMM. RFD 283
//! contains more details about how specs are used throughout the Oxide stack.
//!
//! # Spec format
//!
//! Instance specs describe a VM's virtual mainboard (its CPUs, memory, chipset,
//! and platform details) and collections of virtual hardware components. Some
//! components, such as serial ports, are freestanding; others are split into a
//! "device" frontend, which specifies the virtual hardware interface exposed to
//! guest software, and a backend that provides services (e.g. durable storage
//! or a network interface) to devices from the host system and/or the rest of
//! the rack.
//!
//! Devices and backends are named. Collections of components of a given type
//! are stored in maps from names to component descriptions.
//!
//! # Verification
//!
//! This module has few opinions about what constitues a valid, usable spec: if
//! something deserializes, then as far as this module is concerned, it
//! describes a valid spec. Spec consumers, of course, will generally be more
//! discriminating, e.g. a Propolis server may refuse to start a VM that has
//! a device that names a nonexistent backend.
//!
//! # Exports and versioning
//!
//! Specs are versioned in submodules named `v1`, `v2`, and so forth, each
//! describing a different spec version that may be present on an Oxide rack.
//! There is currently no deprecation or rollback-safety policy for spec
//! versions. New spec versions should implement the [`From`] or
//! [`std::convert::TryFrom`] traits to allow old spec versions to be converted
//! to new specs.
//!
//! This module exports the [`InstanceSpec`] enum to represent a versioned
//! instance specification. It also re-exports the types from the most recent
//! spec version for use in consumer modules (so that they can write, e.g.,
//! `InstanceSpec::StorageDevice` instead of `InstanceSpec::StorageDeviceV1`,
//! unless for some reason they explicitly need to refer to a specific spec
//! version's data types).
//!
//! Specs may sometimes contain fields that Propolis passes through to other
//! services (e.g. a backend may require a backend-specific configuration
//! structure that's defined in the backend's API). To allow backends to have
//! their own compatibility rules and avoid cases where a change to a "foreign"
//! type inadvertently changes an immutable spec definition, instance specs
//! store foreign objects in serialized form instead of including foreign types
//! directly in spec objects.

use serde::{Deserialize, Serialize};

mod common;
pub use common::PciPath;

// N.B.: When adding a new version, update the use statement below (search for
// `_LatestInstanceSpec`) to re-export the latest and greatest spec types.
pub mod v1;

// Re-export the latest and greatest spec types, but shadow off `InstanceSpec`,
// since that name is used in this context to refer to a versioned spec.
pub use v1::{InstanceSpec as LatestInstanceSpec, *};

/// A concrete version of an instance specification.
#[derive(Serialize, Deserialize)]
pub enum InstanceSpec {
    V1(v1::InstanceSpec),
    // Append new versions of the spec to this enum. Old versions should be
    // left in place so that they continue to deserialize.
}
