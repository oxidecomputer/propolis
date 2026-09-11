// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Serial console types for the INITIAL API version.

use std::net::SocketAddr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Request a specific range of an Instance's serial console output history.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct InstanceSerialConsoleHistoryRequest {
    /// Character index in the serial buffer from which to read, counting the
    /// bytes output since instance start. If this is not provided,
    /// `most_recent` must be provided, and if this *is* provided, `most_recent`
    /// must *not* be provided.
    pub from_start: Option<u64>,
    /// Character index in the serial buffer from which to read, counting
    /// *backward* from the most recently buffered data retrieved from the
    /// instance. (See note on `from_start` about mutual exclusivity)
    pub most_recent: Option<u64>,
    /// Maximum number of bytes of buffered serial console contents to return.
    /// If the requested range runs to the end of the available buffer, the data
    /// returned will be shorter than `max_bytes`.
    pub max_bytes: Option<u64>,
}

/// Contents of an Instance's serial console buffer.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceSerialConsoleHistoryResponse {
    /// The bytes starting from the requested offset up to either the end of the
    /// buffer or the request's `max_bytes`. Provided as a u8 array rather than
    /// a string, as it may not be UTF-8.
    pub data: Vec<u8>,
    /// The absolute offset since boot (suitable for use as `byte_offset` in a
    /// subsequent request) of the last byte returned in `data`.
    pub last_byte_offset: u64,
}

/// Connect to an Instance's serial console via websocket, optionally sending
/// bytes from the buffered history first.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct InstanceSerialConsoleStreamRequest {
    /// Character index in the serial buffer from which to read, counting the
    /// bytes output since instance start. If this is provided, `most_recent`
    /// must *not* be provided.
    // TODO: if neither is specified, send enough serial buffer history to
    // reconstruct the current contents and cursor state of an interactive
    // terminal
    pub from_start: Option<u64>,
    /// Character index in the serial buffer from which to read, counting
    /// *backward* from the most recently buffered data retrieved from the
    /// instance. (See note on `from_start` about mutual exclusivity)
    pub most_recent: Option<u64>,
}

/// Control message(s) sent through the websocket to serial console clients.
///
/// Note: Because this is associated with the websocket, and not some REST
/// endpoint, Dropshot lacks the ability to communicate it via the OpenAPI
/// document underpinning the exposed interfaces. As such, clients (including
/// the `propolis-client` crate) are expected to define their own identical copy
/// of this type in order to consume it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum InstanceSerialConsoleControlMessage {
    Migrating { destination: SocketAddr, from_start: u64 },
}
