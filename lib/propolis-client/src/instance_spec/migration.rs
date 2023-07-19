//! Traits for determining whether two instance specs describe VM configurations
//! that allow live migration from a Propolis using one spec to a Propolis using
//! the other.

use std::collections::{HashMap, HashSet};

use crate::instance_spec::{components, SpecKey};
use thiserror::Error;

/// An error type describing mismatches between two spec elements--i.e.,
/// descriptions of individual devices or components--that block migration.
#[derive(Debug, Error)]
pub enum ElementCompatibilityError {
    #[error("component types aren't comparable (self: {0}, other: {1})")]
    ComponentsIncomparable(&'static str, &'static str),

    #[error("boards not compatible: {0}")]
    BoardsIncompatible(#[from] components::board::MigrationCompatibilityError),

    #[error("devices not compatible: {0}")]
    DevicesIncompatible(
        #[from] components::devices::MigrationCompatibilityError,
    ),

    #[error("backends not compatible: {0}")]
    BackendsIncompatible(
        #[from] components::backends::MigrationCompatibilityError,
    ),

    #[cfg(test)]
    #[error("Test components differ")]
    TestComponents(),
}

/// An error type describing a mismatch between collections of elements that
/// blocks migration.
#[derive(Debug, Error)]
pub enum CollectionCompatibilityError {
    #[error(
        "Specs have collections with different lengths (self: {0}, other: {1})"
    )]
    CollectionSize(usize, usize),

    #[error("Collection key {0} present in self but absent from other")]
    CollectionKeyAbsent(SpecKey),

    #[error("Spec element {0} mismatched: {1:?}")]
    SpecElementMismatch(String, ElementCompatibilityError),
}

/// The top-level migration compatibility error type.
#[derive(Debug, Error)]
pub enum MigrationCompatibilityError {
    #[error("Collection {0} not compatible: {1}")]
    CollectionMismatch(String, CollectionCompatibilityError),

    #[error("Spec element {0} not compatible: {1}")]
    ElementMismatch(String, ElementCompatibilityError),
}

/// Implementors of this trait are individual devices or VMM components who can
/// describe inconsistencies using an [`ElementCompatibilityError`] variant.
pub(crate) trait MigrationElement {
    /// Returns a string indicating the kind of component this is for diagnostic
    /// purposes.
    fn kind(&self) -> &'static str;

    /// Returns true if `self` and `other` describe spec elements that are
    /// similar enough to permit migration of this element from one VMM to
    /// another.
    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), ElementCompatibilityError>;
}

/// This trait implements migration compatibility checks for collection types,
/// which can be incompatible either because of a problem with the collection
/// itself or because of problems with one of the collection's members.
pub(crate) trait MigrationCollection {
    fn can_migrate_from_collection(
        &self,
        other: &Self,
    ) -> Result<(), CollectionCompatibilityError>;
}

impl<T: MigrationElement> MigrationCollection for HashMap<SpecKey, T> {
    // Two keyed maps of components are compatible if they contain all the same
    // keys and if, for each key, the corresponding values are
    // migration-compatible.
    fn can_migrate_from_collection(
        &self,
        other: &Self,
    ) -> Result<(), CollectionCompatibilityError> {
        // If the two maps have different sizes, then they have different key
        // sets.
        if self.len() != other.len() {
            return Err(CollectionCompatibilityError::CollectionSize(
                self.len(),
                other.len(),
            ));
        }

        // Each key in `self`'s map must be present in `other`'s map, and the
        // corresponding values must be compatible with one another.
        for (key, this_val) in self.iter() {
            let other_val = other.get(key).ok_or_else(|| {
                CollectionCompatibilityError::CollectionKeyAbsent(key.clone())
            })?;

            this_val.can_migrate_from_element(other_val).map_err(|e| {
                CollectionCompatibilityError::SpecElementMismatch(
                    key.clone(),
                    e,
                )
            })?;
        }

        Ok(())
    }
}

impl MigrationCollection for HashSet<SpecKey> {
    // Two sets of spec keys are compatible if they have all the same members.
    fn can_migrate_from_collection(
        &self,
        other: &Self,
    ) -> Result<(), CollectionCompatibilityError> {
        if self.len() != other.len() {
            return Err(CollectionCompatibilityError::CollectionSize(
                self.len(),
                other.len(),
            ));
        }

        for key in self.iter() {
            if !other.contains(key) {
                return Err(CollectionCompatibilityError::CollectionKeyAbsent(
                    key.clone(),
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TestComponent {
        Widget,
        Gizmo,
        Contraption,
    }

    impl MigrationElement for TestComponent {
        fn can_migrate_from_element(
            &self,
            other: &Self,
        ) -> Result<(), ElementCompatibilityError> {
            if self != other {
                Err(ElementCompatibilityError::TestComponents())
            } else {
                Ok(())
            }
        }

        fn kind(&self) -> &'static str {
            "TestComponent"
        }
    }

    // Verifies that the generic compatibility check for <key, component> maps
    // works correctly with a simple test type.
    #[test]
    fn generic_map_compatibility() {
        let m1: HashMap<SpecKey, TestComponent> = HashMap::from([
            ("widget".to_string(), TestComponent::Widget),
            ("gizmo".to_string(), TestComponent::Gizmo),
            ("contraption".to_string(), TestComponent::Contraption),
        ]);

        let mut m2 = m1.clone();
        assert!(m1.can_migrate_from_collection(&m2).is_ok());

        // Mismatched key counts make two maps incompatible.
        m2.insert("second_widget".to_string(), TestComponent::Widget);
        assert!(m1.can_migrate_from_collection(&m2).is_err());
        m2.remove("second_widget");

        // Two maps are incompatible if their keys refer to components that are
        // not compatible with each other.
        *m2.get_mut("gizmo").unwrap() = TestComponent::Contraption;
        assert!(m1.can_migrate_from_collection(&m2).is_err());
        *m2.get_mut("gizmo").unwrap() = TestComponent::Gizmo;

        // Two maps are incompatible if they have the same number of keys and
        // values, but different sets of key names.
        m2.remove("gizmo");
        m2.insert("other_gizmo".to_string(), TestComponent::Gizmo);
        assert!(m1.can_migrate_from_collection(&m2).is_err());
    }
}
