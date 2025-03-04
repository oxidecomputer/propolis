// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::vmm::MemCtx;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors encountered while trying to export/import device state.
#[derive(Debug, Error)]
pub enum MigrateStateError {
    /// The device doesn't support live migration.
    #[error("device not migratable")]
    NonMigratable,

    /// The device isn't in a state where its state can be exported. Because
    /// fully-initialized devices should be able to pause and export their state
    /// at any time, this generally means that the device was asked to export
    /// its state before it was fully initialized.
    #[error("device's state is not ready to be exported")]
    NotReadyForExport,

    /// I/O Error encounted while performing import/export
    #[error("IO Error")]
    Io(#[from] std::io::Error),

    /// Encountered an error trying to deserialize the device state during import.
    #[error("could not deserialize device state: {0}")]
    DeserializationFailed(String),

    /// The device failed to import the deserialized device state.
    #[error("failed to apply deserialized device state: {0}")]
    ImportFailed(String),

    /// State of kind/version suitable for import not found
    #[error("failed to find suitable import payload")]
    DataMissing,

    /// The kind and/or version of payload was not expected
    #[error("kind/version of payload not expected: {0} v{1}")]
    UnexpectedPayload(String, u32),
}

impl From<erased_serde::Error> for MigrateStateError {
    fn from(err: erased_serde::Error) -> Self {
        MigrateStateError::DeserializationFailed(err.to_string())
    }
}

/// Type representing the migration support (if any) for a given device.
pub enum Migrator<'a> {
    /// The device is not migratable
    NonMigratable,

    /// No device specific logic is needed
    Empty,

    /// Migration state for the device consists of a single payload.
    ///
    /// The device may be capable of handing differing formats and/or versions
    /// of said payload, but only one at a time is expected for a given device
    /// during migration.
    Single(&'a dyn MigrateSingle),

    /// Migration state for the device consists of multiple payloads.  This is
    /// the case for more complex devices where emulation is composed from
    /// several abstractions.
    ///
    /// One example would be pci-virtio-block, where one payload contains to the
    /// emulated PCI state, while another contains the virtio state (virtqueues,
    /// etc).
    Multi(&'a dyn MigrateMulti),
}

/// A device which can be migrated using a single typed payload to represent its
/// internal state.
pub trait MigrateSingle: Send + Sync + 'static {
    fn export(
        &self,
        ctx: &MigrateCtx,
    ) -> Result<PayloadOutput, MigrateStateError>;
    fn import(
        &self,
        offer: PayloadOffer,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError>;
}

/// A device which can be migrated using multiple differently-typed payloads to
/// represent its internal state.
pub trait MigrateMulti: Send + Sync {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError>;
    fn import(
        &self,
        offer: &mut PayloadOffers,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError>;
}

/// Additional (borrowed) context data used during device import and export.
pub struct MigrateCtx<'a> {
    pub mem: &'a MemCtx,
}

/// Device state payload (of a given kind/version) offered to the import logic
/// for that device during a migration.
pub struct PayloadOffer<'a> {
    pub kind: &'a str,
    pub version: u32,
    pub payload: Box<dyn erased_serde::Deserializer<'a> + 'a>,
}
impl<'a> PayloadOffer<'a> {
    /// Attempt to parse the data in this payload if the offer matches the
    /// kind/version of a specified Schema
    pub fn parse<T: Schema<'a>>(&mut self) -> Result<T, MigrateStateError> {
        if !self.matches::<T>() {
            return Err(MigrateStateError::UnexpectedPayload(
                self.kind.into(),
                self.version,
            ));
        }
        let res = erased_serde::deserialize(&mut self.payload)?;
        Ok(res)
    }

    /// Returns `true` if the `kind` and `version` held in this `PayloadOffer`
    /// match those defined for a provided Schema.
    fn matches<'x, T: Schema<'x>>(&self) -> bool {
        let id = T::id();
        id.0 == self.kind && id.1 == self.version
    }
}

/// Collection of [`PayloadOffer`] instances, as provided to device state import
/// logic during a migration.
pub struct PayloadOffers<'a>(Vec<PayloadOffer<'a>>);
impl<'a> PayloadOffers<'a> {
    /// Create new [`PayloadOffers`] from iterator of [`PayloadOffer`]
    /// instances.
    pub fn new(items: impl IntoIterator<Item = PayloadOffer<'a>>) -> Self {
        Self(Vec::from_iter(items))
    }

    /// Attempt to take a payload from the contained offers, provided that it
    /// matches the specified [`Schema`](trait@Schema).
    pub fn take<T: Schema<'a>>(&mut self) -> Result<T, MigrateStateError> {
        self.take_schema(T::id())
            .ok_or_else(|| MigrateStateError::DataMissing)?
            .parse()
    }

    /// Returns `true` if all of the payload offers been consumed via
    /// [`Self::take()`].
    pub fn is_consumed(&self) -> bool {
        self.0.is_empty()
    }

    /// Return the remaining PayloadOffer instances which have not been
    /// consumed.
    ///
    /// Intended for use by the logic driving the migration itself to determine
    /// if a given device importation failed to consume all of its payload(s).
    pub fn remaining(self) -> Remaining<'a> {
        Remaining(self.0.into_iter())
    }

    fn take_schema(&mut self, id: SchemaId) -> Option<PayloadOffer<'a>> {
        let mut search = self.0.iter().enumerate().filter(|(_idx, offer)| {
            offer.kind == id.0 && offer.version == id.1
        });

        // Success if we find one, and only one, matching the criteria
        if let (Some((idx, _offer)), None) = (search.next(), search.next()) {
            return Some(self.0.remove(idx));
        }

        None
    }
}

pub struct Remaining<'a>(std::vec::IntoIter<PayloadOffer<'a>>);
impl<'a> Iterator for Remaining<'a> {
    type Item = PayloadOffer<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// Device state payload (of a given kind/version) output as part of the export
/// logic for that device during a migration.
///
/// Easiest to safely instantiate via the [`From`] trait implemented for
/// [`Schema`] to ensure that the kind/version matches the serialized payload.
pub struct PayloadOutput {
    pub kind: &'static str,
    pub version: u32,
    pub payload: Box<dyn erased_serde::Serialize>,
}

pub struct PayloadOutputs(Vec<PayloadOutput>);
impl PayloadOutputs {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Add a provided [`PayloadOutput`] to be included in the payload(s) for a
    /// given exported device
    pub fn push(
        &mut self,
        output: PayloadOutput,
    ) -> Result<(), MigrateStateError> {
        self.0.push(output);
        Ok(())
    }
}
impl IntoIterator for PayloadOutputs {
    type Item = PayloadOutput;

    type IntoIter = OutputIter;

    fn into_iter(self) -> Self::IntoIter {
        OutputIter(self.0.into_iter())
    }
}
// Hide the internal implementation details of PayloadOutputs by providing a
// wrapper type for its output iterator.
pub struct OutputIter(std::vec::IntoIter<PayloadOutput>);
impl Iterator for OutputIter {
    type Item = PayloadOutput;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// Combination of kind (`&str`) and version (u32) which identifies a specific
/// data schema for device migration state.
pub type SchemaId = (&'static str, u32);

/// Define the type (kind) and version for a migration payload data structure.
pub trait Schema<'de>: Serialize + Deserialize<'de> + Sized + 'static {
    /// The [`SchemaId`] associated with a given device state data type.
    ///
    /// This would be `const` if such functions were allowed in traits without
    /// an unstable rust feature.
    fn id() -> SchemaId;
}

impl<'a, T: Schema<'a>> From<T> for PayloadOutput {
    fn from(value: T) -> Self {
        let id = T::id();
        PayloadOutput { kind: id.0, version: id.1, payload: Box::new(value) }
    }
}
