// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::migrate::MigrateCtx;
use crate::migrate::MigrateStateError;
use crate::migrate::PayloadOffers;
use crate::migrate::PayloadOutputs;
use opte_ioctl::OpteHdl;

/// Handle to the OPTE port backing an XDE-class viona device.
pub struct OptePort {
    port_name: String,
}

impl OptePort {
    pub fn new(port_name: String) -> Self {
        Self { port_name }
    }

    pub(super) fn pause(&self) {
        // TODO-robustness: It seems like we want to return the error, but
        // that's a larger project. See
        // https://github.com/oxidecomputer/propolis/issues/1251.
        let hdl = OpteHdl::open().expect("handled error");
        hdl.pause(&self.port_name).expect("handled error");
    }

    pub(super) fn resume(&self) {
        // TODO-robustness: It seems like we want to return the error, but
        // that's a larger project. See
        // https://github.com/oxidecomputer/propolis/issues/1251.
        let hdl = OpteHdl::open().expect("handled error");
        hdl.resume(&self.port_name).expect("handled error");
    }

    pub(super) fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let port =
            OpteHdl::open().map_err(|error| MigrateStateError::Opte {
                port: self.port_name.clone(),
                operation: "open device",
                error,
            })?;
        let lfts = port.export_lfts(&self.port_name).
            map_err(|error| {
                MigrateStateError::Opte {
                    port: self.port_name.clone(),
                    operation: "export LFTs",
                    error,
                }
            })?;
        let blob = serde_json::to_vec(&lfts).map_err(|error| {
            MigrateStateError::DeserializationFailed(
                format!("Failed to serialize LFTs as JSON: {error}")
            )
        })?;
        let state = migrate::OptePortStateV1 { blob };
        output.push(state.into())
    }

    pub(super) fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let migrate::OptePortStateV1 { blob } = offer.take()?;
        let port =
            OpteHdl::open().map_err(|error| MigrateStateError::Opte {
                port: self.port_name.clone(),
                operation: "open device",
                error,
            })?;
        let export = serde_json::from_slice(&blob).map_err(|error| {
            MigrateStateError::DeserializationFailed(
                format!("Failed to deserialize LFTs as JSON: {error}")
            )
        })?;
        port.import_lfts(&self.port_name, export)
            .map(|_| ())
            .map_err(|error| MigrateStateError::Opte {
                port: self.port_name.clone(),
                operation: "import LFTs",
                error,
            })
    }
}

pub mod migrate {
    use crate::migrate::Schema;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct OptePortStateV1 {
        pub blob: Vec<u8>,
    }

    impl Schema<'_> for OptePortStateV1 {
        fn id() -> crate::migrate::SchemaId {
            ("opte-port", 1)
        }
    }
}
