// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

progenitor::generate_api!(
    spec = "../../openapi/propolis-server.json",
    derives = [schemars::JsonSchema],
    patch = {
        InstanceProperties = {
            derives = [
                Clone,
                schemars::JsonSchema,
                Serialize,
                Deserialize,
                Eq,
                PartialEq,
            ]
        },
        Slot = { derives = [Copy, Clone, schemars::JsonSchema, Serialize, Deserialize] },
    },
);

impl TryFrom<types::PciPath> for propolis_types::PciPath {
    type Error = String;
    fn try_from(value: types::PciPath) -> Result<Self, Self::Error> {
        propolis_types::PciPath::new(value.bus, value.device, value.function)
            .map_err(|e| e.to_string())
    }
}
pub use propolis_types::PciPath;

// Duplicate the parameter types for the endpoints related to the serial console

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct InstanceSerialParams {
    /// Character index in the serial buffer from which to read, counting the bytes output since
    /// instance start. If this is provided, `most_recent` must *not* be provided.
    pub from_start: Option<u64>,
    /// Character index in the serial buffer from which to read, counting *backward* from the most
    /// recently buffered data retrieved from the instance. (See note on `from_start` about mutual
    /// exclusivity)
    pub most_recent: Option<u64>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct InstanceSerialHistoryParams {
    /// Character index in the serial buffer from which to read, counting the bytes output since
    /// instance start. If this is not provided, `most_recent` must be provided, and if this *is*
    /// provided, `most_recent` must *not* be provided.
    pub from_start: Option<u64>,
    /// Character index in the serial buffer from which to read, counting *backward* from the most
    /// recently buffered data retrieved from the instance. (See note on `from_start` about mutual
    /// exclusivity)
    pub most_recent: Option<u64>,
    /// Maximum number of bytes of buffered serial console contents to return. If the requested
    /// range runs to the end of the available buffer, the data returned will be shorter than
    /// `max_bytes`.
    pub max_bytes: Option<u64>,
}
