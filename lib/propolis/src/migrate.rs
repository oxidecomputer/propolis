use crate::vmm::MemCtx;

use erased_serde::{Deserializer, Serialize};
use thiserror::Error;

/// Errors encountered while trying to export/import device state.
#[derive(
    Clone, Debug, Error, serde::Deserialize, PartialEq, serde::Serialize,
)]
pub enum MigrateStateError {
    /// The device doesn't support live migration.
    #[error("device not migratable")]
    NonMigratable,

    /// Encountered an error trying to deserialize the device state during import.
    #[error("couldn't deserialize device state: {0}")]
    ImportDeserialization(String),

    /// The device doesn't implement [`Migrate::import`].
    #[error("device state importation unimplemented for `{0}`")]
    ImportUnimplmented(String),

    /// The device failed to import the deserialized device state.
    #[error("failed to apply deserialized device state: {0}")]
    ImportFailed(String),
}

impl From<erased_serde::Error> for MigrateStateError {
    fn from(err: erased_serde::Error) -> Self {
        MigrateStateError::ImportDeserialization(err.to_string())
    }
}

impl From<std::io::Error> for MigrateStateError {
    fn from(err: std::io::Error) -> Self {
        MigrateStateError::ImportFailed(err.to_string())
    }
}

/// Type representing the migration support (if any) for a given device.
pub enum Migrator<'a> {
    /// The device is not migratable
    NonMigratable,

    /// No device specific logic is needed
    Simple,

    /// Device specific logic used to export/import device state
    Custom(&'a dyn Migrate),
}

pub struct MigrateCtx<'a> {
    pub mem: &'a MemCtx,
}

pub trait Migrate: Send + Sync + 'static {
    /// Return a serialization of the current device state.
    fn export(&self, ctx: &MigrateCtx) -> Box<dyn Serialize>;

    #[allow(unused_variables)]
    /// Update the current device state by using the given deserializer.
    fn import(
        &self,
        dev: &str,
        deserializer: &mut dyn Deserializer,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        Err(MigrateStateError::ImportUnimplmented(dev.to_string()))
    }
}
