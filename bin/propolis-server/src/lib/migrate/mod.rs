// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use bit_field::BitField;
use dropshot::HttpError;
use propolis::migrate::MigrateStateError;
use propolis_api_types::MigrationState;
use serde::{Deserialize, Serialize};
use slog::error;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

mod codec;
mod compat;
pub mod destination;
mod memx;
mod preamble;
pub mod protocol;
pub mod source;

/// Trait bounds for connection objects used in live migrations.
pub(crate) trait MigrateConn:
    AsyncRead + AsyncWrite + Unpin + Send
{
}

impl MigrateConn for tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream> {}
impl MigrateConn for dropshot::WebsocketConnectionRaw {}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MigrateRole {
    Source,
    Destination,
}

// N.B. Keep in sync with scripts/live-migration-times.d.
#[derive(Debug, PartialEq, Eq)]
enum MigratePhase {
    MigrateSync,
    Pause,
    RamPushPrePause,
    RamPushPostPause,
    TimeData,
    DeviceState,
    RamPull,
    ServerState,
    Finish,
}

impl std::fmt::Display for MigratePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MigratePhase::MigrateSync => "Sync",
            MigratePhase::Pause => "Pause",
            MigratePhase::RamPushPrePause => "RamPushPrePause",
            MigratePhase::RamPushPostPause => "RamPushPostPause",
            MigratePhase::TimeData => "TimeData",
            MigratePhase::DeviceState => "DeviceState",
            MigratePhase::RamPull => "RamPull",
            MigratePhase::ServerState => "ServerState",
            MigratePhase::Finish => "Finish",
        };

        write!(f, "{}", s)
    }
}

/// Errors which may occur during the course of a migration
#[derive(Clone, Debug, Error, Deserialize, PartialEq, Serialize)]
pub enum MigrateError {
    /// An error as a result of some Websocket operation (i.e. establishing
    /// or maintaining the connection between the source and destination)
    #[error("Websocket error: {0}")]
    Websocket(String),

    /// Failed to initiate the migration protocol
    #[error("couldn't establish migration connection to source instance")]
    Initiate,

    #[error("failed to parse the offered protocol list ({0}): {1}")]
    ProtocolParse(String, String),

    /// The source and destination instances are not compatible
    #[error("the source ({0}) and destination ({1}) instances have no common protocol")]
    NoMatchingProtocol(String, String),

    /// Incomplete WebSocket upgrade request
    #[error("expected connection upgrade")]
    UpgradeExpected,

    /// Error parsing the contents of the preamble
    #[error("failed to parse preamble: {0}")]
    PreambleParse(String),

    /// Source and target decided their configurations are incompatible
    #[error("instance specs incompatible: {0}")]
    InstanceSpecsIncompatible(String),

    /// Attempted to migrate an uninitialized instance
    #[error("failed to initialize the target VM: {0}")]
    TargetInstanceInitializationFailed(String),

    /// The given UUID does not match the existing instance/migration UUID
    #[error("unexpected Uuid")]
    UuidMismatch,

    /// A different migration already in progress
    #[error("a migration from the current instance is already in progress")]
    MigrationAlreadyInProgress,

    /// Migration state was requested with no migration in process
    #[error("no migration is currently in progress")]
    NoMigrationInProgress,

    /// A VM controller function returned an error
    #[error("VM state machine error: {0}")]
    StateMachine(String),

    /// Encountered an error as part of encoding/decoding migration messages
    #[error("codec error: {0}")]
    Codec(String),

    /// The instance is in an invalid state for the current operation
    #[error("encountered invalid instance state")]
    InvalidInstanceState,

    /// Received a message out of order
    #[error("received unexpected migration message")]
    UnexpectedMessage,

    /// Failed to pause the source instance's devices or tasks
    #[error("failed to pause source instance")]
    SourcePause,

    /// Phase error
    #[error("received out-of-phase message")]
    Phase,

    /// Failed to export/import time data state
    #[error("failed to migrate VMM time data: {0}")]
    TimeData(String),

    /// Failed to export/import device state for migration
    #[error("failed to migrate device state: {0}")]
    DeviceState(String),

    /// The destination instance doesn't recognize the received device
    #[error("received device state for unknown device ({0})")]
    UnknownDevice(String),

    /// The other end of the migration ran into an error
    #[error("{0:?} migration instance encountered error: {1}")]
    RemoteError(MigrateRole, String),
}

impl From<tokio_tungstenite::tungstenite::Error> for MigrateError {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> MigrateError {
        MigrateError::Websocket(err.to_string())
    }
}

impl From<codec::ProtocolError> for MigrateError {
    fn from(err: codec::ProtocolError) -> Self {
        MigrateError::Codec(err.to_string())
    }
}

impl From<MigrateStateError> for MigrateError {
    fn from(value: MigrateStateError) -> Self {
        Self::DeviceState(value.to_string())
    }
}

impl From<MigrateError> for HttpError {
    fn from(err: MigrateError) -> Self {
        let msg = format!("migration failed: {}", err);
        match &err {
            MigrateError::Websocket(_)
            | MigrateError::Initiate
            | MigrateError::ProtocolParse(_, _)
            | MigrateError::NoMatchingProtocol(_, _)
            | MigrateError::TargetInstanceInitializationFailed(_)
            | MigrateError::PreambleParse(_)
            | MigrateError::InstanceSpecsIncompatible(_)
            | MigrateError::InvalidInstanceState
            | MigrateError::Codec(_)
            | MigrateError::UnexpectedMessage
            | MigrateError::SourcePause
            | MigrateError::Phase
            | MigrateError::TimeData(_)
            | MigrateError::DeviceState(_)
            | MigrateError::RemoteError(_, _)
            | MigrateError::StateMachine(_) => {
                HttpError::for_internal_error(msg)
            }
            MigrateError::MigrationAlreadyInProgress
            | MigrateError::NoMigrationInProgress
            | MigrateError::UuidMismatch
            | MigrateError::UpgradeExpected
            | MigrateError::UnknownDevice(_) => {
                HttpError::for_bad_request(None, msg)
            }
        }
    }
}

/// Serialized device state sent during migration.
#[derive(Debug, Deserialize, Serialize)]
struct Device {
    /// The unique name identifying the device in the instance inventory.
    pub instance_name: String,

    /// Device state data
    pub payload: Vec<DevicePayload>,
}
#[derive(Debug, Deserialize, Serialize)]
struct DevicePayload {
    /// Payload schema type
    pub kind: String,

    /// Payload schema version
    pub version: u32,

    /// Serialized device state.
    pub data: String,
}

// We should probably turn this into some kind of ValidatedBitmap
// data structure, so that we're only parsing it once.
struct PageIter<'a> {
    start: u64,
    current: u64,
    end: u64,
    bits: &'a [u8],
}

impl<'a> PageIter<'a> {
    pub fn new(start: u64, end: u64, bits: &'a [u8]) -> PageIter<'a> {
        let current = start;
        PageIter { start, current, end, bits }
    }
}

impl Iterator for PageIter<'_> {
    type Item = u64;
    fn next(&mut self) -> Option<Self::Item> {
        while self.current < self.end {
            let addr = self.current;
            self.current += 4096;
            let page_offset = ((addr - self.start) / 4096) as usize;
            let b = self.bits[page_offset / 8];
            if b.get_bit(page_offset % 8) {
                return Some(addr);
            }
        }
        None
    }
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn migrate_phase_begin(step_desc: &str) {}
    fn migrate_phase_end(step_desc: &str) {}
    fn migrate_xfer_ram_region(pages: u64, size: u64, paused: u8) {}
    fn migrate_xfer_ram_page(addr: u64, size: u64) {}
    fn migrate_time_data_before(
        src_guest_freq: u64,
        src_guest_tsc: u64,
        src_boot_hrtime: i64,
    ) {
    }
    fn migrate_time_data_after(
        dst_guest_freq: u64,
        dst_guest_tsc: u64,
        dst_boot_hrtime: i64,
        guest_uptime: u64,
        migrate_delta_ns: u64,
        migrate_delta_negative: bool,
    ) {
    }
}
