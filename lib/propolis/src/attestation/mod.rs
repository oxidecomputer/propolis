// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Context;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV6};

use slog::{error, info, Logger};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use dice_verifier::sled_agent::AttestSledAgent;
use dice_verifier::Attest;
use vm_attest::VmInstanceAttester;
use vm_attest::{
    Measurement, Request, Response, VmInstanceConf, VmInstanceRot,
};

// See: https://github.com/oxidecomputer/oana
pub const ATTESTATION_PORT: u16 = 605;
pub const ATTESTATION_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), ATTESTATION_PORT);

pub struct AttestationSock {
    join_hdl: JoinHandle<()>,
    hup_send: oneshot::Sender<()>,
}

// TODO:
//

impl AttestationSock {
    pub async fn new(log: Logger, sa_addr: SocketAddrV6) -> io::Result<Self> {
        info!(log, "attestation server created");
        let listener = TcpListener::bind(ATTESTATION_ADDR).await?;
        let (hup_send, hup_recv) = oneshot::channel::<()>();
        let join_hdl = tokio::spawn(async move {
            Self::run(log, listener, hup_recv, sa_addr).await;
        });
        Ok(Self { join_hdl, hup_send })
    }

    pub async fn halt(self) {
        let Self { join_hdl, hup_send } = self;

        // Signal the socket listener to hang up, then wait for it to bail
        let _ = hup_send.send(());
        let _ = join_hdl.await;
    }

    async fn handle_conn(
        log: Logger,
        conn: TcpStream,
        sa_addr: SocketAddrV6,
    ) -> anyhow::Result<()> {
        info!(log, "handling connection");

        let mut msg = String::new();

        const MAX_LINE_LENGTH: usize = 1024;
        let (reader, mut writer) = tokio::io::split(conn);
        let mut reader = BufReader::with_capacity(MAX_LINE_LENGTH, reader);

        // XXX: these are hard-coded qualifying data values that match a test OS image
        // https://github.com/oxidecomputer/vm-attest-demo/blob/main/test-data/vm-instance-cfg.json
        let uuid: Uuid = "db5bf54c-48c5-4455-a1e1-6c7dfc26e351"
            .parse()
            .context("couldn't parse uuid")?;
        let img_digest: Measurement =
            "sha-256;be4df4e085175f3de0c8ac4837e1c2c9a34e8983209dac6b549e94154f7cdd9c"
                .parse()
                .context("couldn't parse boot digest")?;

        let vm_conf =
            VmInstanceConf { uuid, boot_digest: Some(img_digest) };
        let ox_attest: Box<dyn Attest> =
            Box::new(AttestSledAgent::new(sa_addr, &log));
        let rot = VmInstanceRot::new(ox_attest, vm_conf);

        loop {
            let bytes_read = reader.read_line(&mut msg).await?;
            if bytes_read == 0 {
                break;
            }

            // Check if the limit was hit and a newline wasn't found
            if bytes_read == MAX_LINE_LENGTH
                && !msg.ends_with('\n')
            {
                slog::warn!(
                    log,
                    "Line length exceeded the limit of {} bytes.",
                    MAX_LINE_LENGTH
                );
                let response =
                    Response::Error("Request too long".to_string());
                let mut response =
                    serde_json::to_string(&response)?;
                response.push('\n');
                slog::info!(
                    log,
                    "sending error response: {response}"
                );
                writer
                    .write_all(response.as_bytes())
                    .await?;
                break;
            }

            slog::debug!(log, "JSON received: {msg}");

            let result: Result<Request, serde_json::Error> =
                serde_json::from_str(&msg);
            let request = match result {
                Ok(q) => q,
                Err(e) => {
                    let response =
                        Response::Error(e.to_string());
                    let mut response =
                        serde_json::to_string(&response)?;
                    response.push('\n');
                    slog::info!(
                        log,
                        "sending error response: {response}"
                    );
                    writer
                        .write_all(response.as_bytes())
                        .await?;
                    break;
                }
            };

            let response = match request {
                Request::Attest(q) => {
                    slog::debug!(
                        log,
                        "qualifying data received: {q:?}"
                    );
                    match rot.attest(&q) {
                        Ok(a) => Response::Attest(a),
                        Err(e) => {
                            Response::Error(e.to_string())
                        }
                    }
                }
            };

            let mut response =
                serde_json::to_string(&response)?;
            response.push('\n');

            slog::debug!(
                log,
                "sending response: {response}"
            );
            writer.write_all(response.as_bytes()).await?;
            msg.clear();
        }

        info!(log, "ALL DONE");
        Ok(())
    }

    pub async fn run(
        log: Logger,
        listener: TcpListener,
        mut hup_recv: oneshot::Receiver<()>,
        sa_addr: SocketAddrV6,
    ) {
        info!(log, "attestation server running");

        loop {
            tokio::select! {
                biased;

                _ = &mut hup_recv => {
                    return;
                },
                sock_res = listener.accept() => {
                    info!(log, "new client connected");
                    match sock_res {
                        Ok((sock, _addr)) => {
                            let log = log.clone();
                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_conn(log.clone(), sock, sa_addr).await {
                                    slog::error!(log, "handle_conn error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            error!(log, "Attestation TCP listener error: {:?}", e);
                        }
                    }
                },
            };
        }
    }
}
