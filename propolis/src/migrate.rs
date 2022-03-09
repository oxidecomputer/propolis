use crate::dispatch::DispCtx;

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
}

impl From<erased_serde::Error> for MigrateStateError {
    fn from(err: erased_serde::Error) -> Self {
        MigrateStateError::ImportDeserialization(err.to_string())
    }
}

pub trait Migrate: Send + Sync + 'static {
    /// Return a serialization of the current device state.
    fn export(&self, ctx: &DispCtx) -> Box<dyn Serialize>;

    /// Update the current device state by using the given deserializer.
    fn import(
        &self,
        dev: &str,
        _deserializer: &mut dyn Deserializer,
        _ctx: &DispCtx,
    ) -> Result<(), MigrateStateError> {
        Err(MigrateStateError::ImportUnimplmented(dev.to_string()))
    }
}
