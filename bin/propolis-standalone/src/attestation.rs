// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

use dice_verifier::ipcc::AttestIpcc;
use dice_verifier::AttestMock;
use vm_attest::mock::VmInstanceRotMock;
use vm_attest::{Measurement, VmInstanceConf};
use vm_attest::{Request, Response, VmInstanceRot};

use crate::config::{AttestationBackend, AttestationConfig};

const MAX_LINE_LENGTH: usize = 1024;

pub fn parse_cfg(cfg: AttestationConfig) -> Result<VmInstanceRotMock> {
    let uuid = uuid::Uuid::parse_str(&cfg.instance_uuid).expect("Invalid UUID");
    let measurement: Measurement =
        serde_json::from_value(serde_json::json!({"sha-256": cfg.boot_digest}))
            .context("boot_digest must be a valid hex SHA-256 digest")?;
    let vm_conf = VmInstanceConf { uuid, image_digest: Some(measurement) };

    let ox_attest: Box<dyn dice_verifier::Attest> = match cfg.backend {
        AttestationBackend::Mock => {
            let pki_path = cfg
                .pki_path
                .as_ref()
                .expect("pki_path required for mock backend");
            let log_path = cfg
                .log_path
                .as_ref()
                .expect("log_path required for mock backend");
            let alias_key_path = cfg
                .alias_key_path
                .as_ref()
                .expect("alias_key_path required for mock backend");
            Box::new(
                AttestMock::load(pki_path, log_path, alias_key_path)
                    .expect("Failed to load AttestMock"),
            )
        }
        AttestationBackend::Ipcc => {
            Box::new(AttestIpcc::new().expect("Failed to create AttestIpcc"))
        }
    };

    Ok(VmInstanceRotMock::new(ox_attest, vm_conf))
}

pub fn run_server(
    log: &slog::Logger,
    rot: VmInstanceRotMock,
    listener: TcpListener,
) -> Result<()> {
    let mut msg = String::new();
    for client in listener.incoming() {
        slog::info!(log, "new client connected");

        // create `BufReader` w/ capacity & `take` reader w/ same limit
        let reader = BufReader::with_capacity(MAX_LINE_LENGTH, client?);
        let mut limited_reader = reader.take(MAX_LINE_LENGTH as u64);

        slog::info!(log, "LISTENING");

        loop {
            let bytes_read = limited_reader.read_line(&mut msg)?;

            if bytes_read == 0 {
                break;
            }

            // Check if the limit was hit and a newline wasn't found
            if bytes_read == MAX_LINE_LENGTH && !msg.ends_with('\n') {
                slog::warn!(
                    log,
                    "Error: Line length exceeded the limit of {} bytes.",
                    MAX_LINE_LENGTH
                );
                let response = Response::Error("Request too long".to_string());
                let mut response = serde_json::to_string(&response)?;
                response.push('\n');
                slog::info!(log, "sending error response: {response}");
                limited_reader
                    .get_mut()
                    .get_mut()
                    .write_all(response.as_bytes())?;
                break;
            }

            slog::debug!(log, "JSON received: {msg}");

            let result: Result<Request, serde_json::Error> =
                serde_json::from_str(&msg);
            let request = match result {
                Ok(q) => q,
                Err(e) => {
                    let response = Response::Error(e.to_string());
                    let mut response = serde_json::to_string(&response)?;
                    response.push('\n');
                    slog::info!(log, "sending error response: {response}");
                    limited_reader
                        .get_mut()
                        .get_mut()
                        .write_all(response.as_bytes())?;
                    break;
                }
            };

            let response = match request {
                Request::Attest(q) => {
                    slog::debug!(log, "qualifying data received: {q:?}");
                    match rot.attest(&q) {
                        Ok(a) => Response::Attest(a),
                        Err(e) => Response::Error(e.to_string()),
                    }
                }
            };

            let mut response = serde_json::to_string(&response)?;
            response.push('\n');

            slog::debug!(log, "sending response: {response}");
            limited_reader
                .get_mut()
                .get_mut()
                .write_all(response.as_bytes())?;
            msg.clear();
        }
    }

    Ok(())
}
