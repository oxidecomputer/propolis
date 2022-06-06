//! Routines and types for saving and restoring a snapshot of a VM.

use std::sync::Arc;

use propolis::instance::{Instance, ReqState};

pub fn save(log: slog::Logger, inst: Arc<Instance>) -> anyhow::Result<()> {

    // Clean up instance.
    inst.set_target_state(ReqState::Halt)?;

    Ok(())
}
