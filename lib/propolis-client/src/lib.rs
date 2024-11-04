// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A client for the Propolis hypervisor frontend's server API.

pub use propolis_api_types::instance_spec::{PciPath, SpecKey};

progenitor::generate_api!(
    spec = "../../openapi/propolis-server.json",
    interface = Builder,
    tags = Separate,
    replace = {
        PciPath = crate::PciPath,
        SpecKey = crate::SpecKey,
    },
    patch = {
        // Some Crucible-related bits are re-exported through simulated
        // sled-agent and thus require JsonSchema
        BootOrderEntry = { derives = [schemars::JsonSchema] },
        BootSettings = { derives = [Default, schemars::JsonSchema] },
        CpuidEntry = { derives = [PartialEq, Eq, Copy] },
        DiskRequest = { derives = [schemars::JsonSchema] },
        VolumeConstructionRequest = { derives = [schemars::JsonSchema] },
        CrucibleOpts = { derives = [schemars::JsonSchema] },
        Slot = { derives = [schemars::JsonSchema] },
        InstanceMetadata = { derives = [ PartialEq ] },
    }
);

pub mod support;

impl types::InstanceSpecV0 {
    /// Returns an instance spec with the following configuration:
    ///
    /// - CPUs and memory set to their supplied values
    /// - Serial devices named "com1", "com2", "com3", and "com4" attached to
    ///   the components
    pub fn with_basic_config(cpus: u8, memory_mb: u64) -> Self {
        use types::*;
        Self {
            board: Board {
                chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
                cpuid: None,
                cpus,
                memory_mb,
            },
            components: [
                (
                    "com1".to_string(),
                    ComponentV0::SerialPort(SerialPort {
                        num: SerialPortNumber::Com1,
                    }),
                ),
                (
                    "com2".to_string(),
                    ComponentV0::SerialPort(SerialPort {
                        num: SerialPortNumber::Com2,
                    }),
                ),
                (
                    "com3".to_string(),
                    ComponentV0::SerialPort(SerialPort {
                        num: SerialPortNumber::Com3,
                    }),
                ),
                (
                    "com4".to_string(),
                    ComponentV0::SerialPort(SerialPort {
                        num: SerialPortNumber::Com4,
                    }),
                ),
            ]
            .into_iter()
            .collect(),
        }
    }
}
