use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use sha2::{Digest, Sha256};
use vm_attest_proto::mock::VmInstanceRotMock;
use vm_attest_proto::{QualifyingData, Response, VmInstanceRot};

fn handle_client(
    mut stream: TcpStream,
    rot: &VmInstanceRotMock,
    log: &slog::Logger,
) {
    let mut buffer = [0; 1024];

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => {
                slog::info!(log, "Client disconnected");
                break;
            }
            Ok(n) => {
                let received_data = &buffer[..n];
                slog::info!(log, "Received {} bytes from client", n);

                let mut hasher = Sha256::new();
                hasher.update(received_data);
                let digest: [u8; 32] = hasher.finalize().into();
                let qualifying_data = QualifyingData::from(digest);

                match rot.attest(&qualifying_data) {
                    Ok(attestation) => {
                        slog::info!(
                            log,
                            "Attestation generated successfully";
                            "cert_chain_len" => attestation.cert_chain.len(),
                            "measurement_logs_len" => attestation.measurement_logs.len(),
                            "attestation_len" => attestation.attestation.len(),
                        );

                        let response = Response::Success(attestation);
                        let response = serde_json::to_vec(&response).unwrap();
                        if let Err(e) = stream.write_all(&response) {
                            slog::error!(
                                log,
                                "Failed to send attestation: {}",
                                e
                            );
                        }
                        stream.write(b"\n");
                    }
                    Err(e) => {
                        slog::error!(
                            log,
                            "Failed to generate attestation: {:?}",
                            e
                        );
                    }
                }
            }
            Err(e) => {
                slog::error!(log, "Error reading from stream: {}", e);
                break;
            }
        }
    }
}

pub fn run_server(
    log: &slog::Logger,
    mut rot: VmInstanceRotMock,
) -> std::io::Result<()> {
    let listener = TcpListener::bind("0.0.0.0:3000")?;
    slog::info!(log, "Attestation server listening on port 3000");

    for stream in listener.incoming() {
        slog::info!(log, "incoming listener");
        match stream {
            Ok(stream) => {
                slog::info!(log, "New client: {}", stream.peer_addr().unwrap());
                handle_client(stream, &mut rot, log);
            }
            Err(e) => slog::error!(log, "Connection failed: {}", e),
        }
    }

    Ok(())
}
