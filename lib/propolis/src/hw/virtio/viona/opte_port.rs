// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::migrate::MigrateCtx;
use crate::migrate::MigrateStateError;
use crate::migrate::PayloadOffers;
use crate::migrate::PayloadOutputs;
use opte_ioctl::OpteHdl;
use slog::debug;
use slog::error;
use slog::trace;
use slog::Logger;
use slog_error_chain::InlineErrorChain;

/// Handle to the OPTE port backing an XDE-class viona device.
pub struct OptePort {
    port_name: String,
    log: Logger,
}

impl OptePort {
    pub fn new(port_name: String, log: &Logger) -> Self {
        let log = log.new(slog::o!("port" => port_name.clone()));
        Self { port_name, log }
    }

    // TODO-robustness: It seems like we want to return an error from the
    // methods here, rather than panicking, but that's a larger project. See
    // https://github.com/oxidecomputer/propolis/issues/1251.
    fn opte_handle(&self) -> OpteHdl {
        match OpteHdl::open() {
            Ok(hdl) => {
                trace!(self.log, "Opened OPTE handle");
                hdl
            }
            Err(e) => {
                error!(
                    self.log,
                    "error opening OPTE handle";
                    InlineErrorChain::new(&e),
                );
                panic!("failed to open OPTE handle: {e}");
            }
        }
    }

    pub(super) fn pause(&self) {
        let hdl = self.opte_handle();
        if let Err(e) = hdl.pause(&self.port_name) {
            error!(
                self.log,
                "failed to pause OPTE port";
                InlineErrorChain::new(&e),
            );
            panic!("failed to pause OPTE port: {e}");
        };
        debug!(self.log, "paused OPTE port");
    }

    pub(super) fn resume(&self) {
        let hdl = self.opte_handle();
        if let Err(e) = hdl.resume(&self.port_name) {
            error!(
                self.log,
                "failed to resume OPTE port";
                InlineErrorChain::new(&e),
            );
            panic!("failed to resume OPTE port: {e}");
        };
        debug!(self.log, "resumed OPTE port");
    }

    pub(super) fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let hdl = OpteHdl::open().map_err(|error| MigrateStateError::Opte {
            port: self.port_name.clone(),
            operation: "open device",
            error,
        })?;
        let lfts = hdl.export_lfts(&self.port_name).map_err(|error| {
            MigrateStateError::Opte {
                port: self.port_name.clone(),
                operation: "export LFTs",
                error,
            }
        })?;
        let blob = serde_json::to_vec(&lfts).map_err(|error| {
            MigrateStateError::DeserializationFailed(format!(
                "Failed to serialize LFTs as JSON: {error}"
            ))
        })?;
        let state = migrate::OptePortStateV1 { blob };
        debug!(self.log, "exported OPTE state"; "lfts" => ?lfts);
        output.push(state.into())
    }

    pub(super) fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let migrate::OptePortStateV1 { blob } = offer.take()?;
        let hdl = OpteHdl::open().map_err(|error| MigrateStateError::Opte {
            port: self.port_name.clone(),
            operation: "open device",
            error,
        })?;
        let export = serde_json::from_slice(&blob).map_err(|error| {
            MigrateStateError::DeserializationFailed(format!(
                "Failed to deserialize LFTs as JSON: {error}"
            ))
        })?;
        // NOTE: Import only works on ports in the `Ready` state, so reset it
        // to that state first.
        hdl.pause(&self.port_name).map_err(|error| {
            MigrateStateError::Opte {
                port: self.port_name.clone(),
                operation: "reset port",
                error,
            }
        })?;
        debug!(self.log, "importing OPTE state"; "lfts" => ?&export);
        hdl.import_lfts(&self.port_name, export).map_err(|error| {
            MigrateStateError::Opte {
                port: self.port_name.clone(),
                operation: "import LFTs",
                error,
            }
        })?;
        debug!(self.log, "imported OPTE state");
        Ok(())
    }
}

pub mod migrate {
    use crate::migrate::Schema;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct OptePortStateV1 {
        #[serde(with = "serde_bytes")]
        pub blob: Vec<u8>,
    }

    impl Schema<'_> for OptePortStateV1 {
        fn id() -> crate::migrate::SchemaId {
            ("opte-port", 1)
        }
    }
}
