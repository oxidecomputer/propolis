use anyhow::Result;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

use vm_attest_proto::mock::VmInstanceRotMock;
use vm_attest_proto::{QualifyingData, Response, VmInstanceRot};

const MAX_LINE_LENGTH: usize = 1024;
pub fn run_server(log: &slog::Logger, rot: VmInstanceRotMock) -> Result<()> {
    let listener = TcpListener::bind("0.0.0.0:3000")?;
    slog::info!(log, "Attestation server listening on port 3000");

    let mut msg = String::new();
    for client in listener.incoming() {
        slog::info!(log, "new client connected");

        // create `BufReader` w/ capacity & `take` reader w/ same limit
        let reader = BufReader::with_capacity(MAX_LINE_LENGTH, client?);
        let mut limited_reader = reader.take(MAX_LINE_LENGTH as u64);

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

            let result: Result<QualifyingData, serde_json::Error> =
                serde_json::from_str(&msg);
            let qualifying_data = match result {
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
            slog::debug!(log, "qualifying data received: {qualifying_data:?}");

            let response = match rot.attest(&qualifying_data) {
                Ok(a) => Response::Success(a),
                Err(e) => Response::Error(e.to_string()),
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
