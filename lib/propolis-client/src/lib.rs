// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A client for the Propolis hypervisor frontend's server API.

progenitor::generate_api!(
    spec = "../../openapi/propolis-server.json",
    interface = Builder,
    tags = Separate,
    patch = {
        // Add `Default` to types related to instance specs
        InstanceSpecV0 = { derives = [Default] },
        Board = { derives = [Default] },

        // Some Crucible-related bits are re-exported through simulated
        // sled-agent and thus require JsonSchema
        BootOrderEntry = { derives = [schemars::JsonSchema] },
        BootSettings = { derives = [Default, schemars::JsonSchema] },
        DiskRequest = { derives = [schemars::JsonSchema] },
        VolumeConstructionRequest = { derives = [schemars::JsonSchema] },
        CrucibleOpts = { derives = [schemars::JsonSchema] },
        Slot = { derives = [schemars::JsonSchema] },

        PciPath = { derives = [
            Copy, Ord, Eq, PartialEq, PartialOrd
        ] },

        InstanceMetadata = { derives = [ PartialEq ] },
    }
);

pub mod support;
