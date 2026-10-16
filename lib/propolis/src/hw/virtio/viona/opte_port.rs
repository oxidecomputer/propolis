// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::common::Lifecycle;
use crate::migrate::MigrateCtx;
use crate::migrate::MigrateStateError;
use crate::migrate::Migrator;
use crate::migrate::PayloadOffers;
use crate::migrate::PayloadOutputs;
use futures::future::BoxFuture;
use futures::FutureExt;
use sled_agent_client::types::SledIdentifiers;
use slog::info;
use std::future;
use std::net::SocketAddrV6;
use std::sync::Arc;
use std::sync::Mutex;

/// Handle to the OPTE port backing an XDE-class viona device.
pub struct OptePort {
    port_name: String,

    // TODO remove once we have a real backend. do we need to track anything
    // else here? (handle to the xde device to issue ioctls?)
    data_to_migrate: Arc<Mutex<Option<String>>>,
    sled_agent_client: sled_agent_client::Client,
    log: slog::Logger,
}

impl OptePort {
    pub fn new(
        port_name: String,
        sled_agent_addr: SocketAddrV6,
        log: &slog::Logger,
    ) -> Self {
        let log = log.new(slog::o!("port" => port_name.clone()));
        let sled_agent_client = sled_agent_client::Client::new(
            &format!("http://{sled_agent_addr}"),
            log.clone(),
        );
        Self {
            port_name,
            data_to_migrate: Arc::default(),
            sled_agent_client,
            log,
        }
    }

    pub(super) fn paused(&self) -> BoxFuture<'static, ()> {
        let data_to_migrate = Arc::clone(&self.data_to_migrate);
        let sled_agent_client = self.sled_agent_client.clone();
        async move {
            let s = match sled_agent_client.sled_identifiers().await {
                Ok(resp) => {
                    let SledIdentifiers {
                        model: _,
                        rack_id,
                        revision: _,
                        serial,
                        sled_id,
                    } = resp.into_inner();
                    format!("sled {serial}: sled {sled_id} in rack {rack_id}")
                }
                Err(err) => {
                    format!(
                        "err contacting sled-agent: {:#}",
                        anyhow::anyhow!(err)
                    )
                }
            };
            *data_to_migrate.lock().unwrap() = Some(s);
        }
        .boxed()
    }

    pub(super) fn pause(&self) {
        info!(self.log, "TODO: pause OPTE port?");
    }

    pub(super) fn resume(&self) {
        info!(self.log, "TODO: resume OPTE port?");
    }

    pub(super) fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let message =
            self.data_to_migrate.lock().unwrap().take().unwrap_or_else(|| {
                "NO DATA - was paused() not called?".to_string()
            });
        info!(self.log, "TODO: exporting fake OPTE state"; "msg" => &message);
        let state =
            migrate::OptePortStateV1 { blob: message.as_bytes().to_vec() };
        output.push(state.into())
    }

    pub(super) fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let input: migrate::OptePortStateV1 = offer.take()?;
        let message = String::from_utf8_lossy(&input.blob);
        info!(self.log, "TODO: importing fake OPTE state"; "msg" => message);
        Ok(())
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
