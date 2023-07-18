//! Specifications for components that can be attached to a Propolis VM.
//!
//! # Versioning and compatibility
//!
//! Components are 'versionless' and can be added to any specification of any
//! format. Existing components must only change in backward-compatible ways
//! (i.e. so that old versions of the component can deserialize into an
//! equivalent new-version component). If possible, changes to a new component
//! should be expressed such that older versions of the component are forward-
//! compatible with the new version (i.e. such that the new component will
//! serialize, if possible, into a form that can be deserialized by an old
//! version of this library into an equivalent old-version component).

pub mod backends;
pub mod board;
pub mod devices;
