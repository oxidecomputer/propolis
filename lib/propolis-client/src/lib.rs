// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A client for the Propolis hypervisor frontend's server API.

#[cfg(not(feature = "falcon"))]
progenitor::generate_api!(
    spec = "../../openapi/propolis-server.json",
    interface = Builder,
    tags = Separate,
    patch = {
        // Add `Default` to types related to instance specs
        InstanceSpecV0 = { derives = [Clone, Debug, Default, Serialize, Deserialize] },
        BackendSpecV0 = { derives = [Clone, Debug, Default, Serialize, Deserialize] },
        DeviceSpecV0 = { derives = [Clone, Debug, Default, Serialize, Deserialize] },
        Board = { derives = [Clone, Debug, Default, Serialize, Deserialize] },

        // Some Crucible-related bits are re-exported through simulated
        // sled-agent and thus require JsonSchema
        DiskRequest = { derives = [Clone, Debug, schemars::JsonSchema, Serialize, Deserialize] },
        VolumeConstructionRequest = { derives = [Clone, Debug, schemars::JsonSchema, Serialize, Deserialize] },
        CrucibleOpts = { derives = [Clone, Debug, schemars::JsonSchema, Serialize, Deserialize] },
        Slot = { derives = [Copy, Clone, Debug, schemars::JsonSchema, Serialize, Deserialize] },

        PciPath = { derives = [
            Copy, Clone, Debug, Ord, Eq, PartialEq, PartialOrd, Serialize, Deserialize
        ] },

        InstanceMetadata = { derives = [ PartialEq ] },
    }
);

#[cfg(feature = "falcon")]
progenitor::generate_api!(
    spec = "../../openapi/propolis-server-falcon.json",
    interface = Builder,
    tags = Separate,
    patch = {
        // Add `Default` to types related to instance specs
        InstanceSpecV0 = { derives = [Clone, Debug, Default, Serialize, Deserialize] },
        BackendSpecV0 = { derives = [Clone, Debug, Default, Serialize, Deserialize] },
        DeviceSpecV0 = { derives = [Clone, Debug, Default, Serialize, Deserialize] },
        Board = { derives = [Clone, Debug, Default, Serialize, Deserialize] },

        // Some Crucible-related bits are re-exported through simulated
        // sled-agent and thus require JsonSchema
        DiskRequest = { derives = [Clone, Debug, schemars::JsonSchema, Serialize, Deserialize] },
        VolumeConstructionRequest = { derives = [Clone, Debug, schemars::JsonSchema, Serialize, Deserialize] },
        CrucibleOpts = { derives = [Clone, Debug, schemars::JsonSchema, Serialize, Deserialize] },
        Slot = { derives = [Copy, Clone, Debug, schemars::JsonSchema, Serialize, Deserialize] },

        PciPath = { derives = [
            Copy, Clone, Debug, Ord, Eq, PartialEq, PartialOrd, Serialize, Deserialize
        ] },

        InstanceMetadata = { derives = [ PartialEq ] },
    }
);

pub mod instance_spec;
pub mod support;
