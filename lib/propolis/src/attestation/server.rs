// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io;
use std::net::SocketAddrV6;
use std::sync::Arc;
use std::sync::Mutex;

use slog::{error, info, Logger};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use dice_verifier::sled_agent::AttestSledAgent;
use dice_verifier::Attest;

use vm_attest::VmInstanceAttester;

use crate::attestation::ATTESTATION_ADDR;

#[derive(Copy, Clone)]
pub struct AttestationServerConfig {
    pub sled_agent_addr: SocketAddrV6,
}

impl AttestationServerConfig {
    pub fn new(sled_agent_addr: SocketAddrV6) -> Self {
        Self { sled_agent_addr }
    }
}

/// TODO: comment
pub struct AttestationSock {
    join_hdl: JoinHandle<()>,
    hup_send: oneshot::Sender<()>,
}

impl AttestationSock {
    pub async fn new(log: Logger, sa_addr: SocketAddrV6) -> io::Result<Self> {
        info!(log, "attestation server created");
        let listener = TcpListener::bind(ATTESTATION_ADDR).await?;
        let (vm_conf_send, vm_conf_recv) =
            oneshot::channel::<vm_attest::VmInstanceConf>();
        let (hup_send, hup_recv) = oneshot::channel::<()>();
        let join_hdl = tokio::spawn(async move {
            Self::run(log, listener, vm_conf_recv, hup_recv, sa_addr).await;
        });
        Ok(Self { join_hdl, hup_send })
    }

    pub async fn halt(self) {
        let Self { join_hdl, hup_send } = self;

        // Signal the socket listener to hang up, then wait for it to bail
        let _ = hup_send.send(());
        let _ = join_hdl.await;
    }

    // Handle an incoming connection to the attestation port.
    async fn handle_conn(
        log: Logger,
        rot: Arc<Mutex<vm_attest::VmInstanceRot>>,
        vm_conf: Arc<Mutex<Option<vm_attest::VmInstanceConf>>>,
        conn: TcpStream,
    ) -> anyhow::Result<()> {
        info!(log, "handling connection");

        let mut msg = String::new();

        const MAX_LINE_LENGTH: usize = 1024;
        let (reader, mut writer) = tokio::io::split(conn);
        let mut reader = BufReader::with_capacity(MAX_LINE_LENGTH, reader);

        loop {
            let bytes_read = reader.read_line(&mut msg).await?;
            if bytes_read == 0 {
                break;
            }

            // Check if the limit was hit and a newline wasn't found
            if bytes_read == MAX_LINE_LENGTH && !msg.ends_with('\n') {
                slog::warn!(
                    log,
                    "Line length exceeded the limit of {} bytes.",
                    MAX_LINE_LENGTH
                );
                let response =
                    vm_attest::Response::Error("Request too long".to_string());
                let mut response = serde_json::to_string(&response)?;
                response.push('\n');
                slog::info!(log, "sending error response: {response}");
                writer.write_all(response.as_bytes()).await?;
                break;
            }

            slog::debug!(log, "JSON received: {msg}");

            let result: Result<vm_attest::Request, serde_json::Error> =
                serde_json::from_str(&msg);
            let request = match result {
                Ok(q) => q,
                Err(e) => {
                    let response = vm_attest::Response::Error(e.to_string());
                    let mut response = serde_json::to_string(&response)?;
                    response.push('\n');
                    slog::info!(log, "sending error response: {response}");
                    writer.write_all(response.as_bytes()).await?;
                    break;
                }
            };

            let response = match request {
                vm_attest::Request::Attest(q) => {
                    slog::debug!(log, "qualifying data received: {q:?}");

                    let guard = vm_conf.lock().unwrap();
                    let conf = guard.clone();
                    drop(guard);

                    match conf {
                        Some(conf) => {
                            info!(log, "vm conf is ready = {:?}", conf);

                            match rot.lock().unwrap().attest(&conf, &q) {
                                Ok(a) => vm_attest::Response::Attest(a),
                                Err(e) => {
                                    vm_attest::Response::Error(e.to_string())
                                }
                            }
                        }

                        // The VM conf isn't ready yet.
                        None => {
                            info!(log, "vm conf is NOT ready");
                            let response = vm_attest::Response::Error(
                                "VmInstanceConf not ready".to_string(),
                            );
                            //let mut response =
                            //serde_json::to_string(&response)?;
                            //response.push('\n');
                            response
                        }
                    }
                }
            };

            let mut response = serde_json::to_string(&response)?;
            response.push('\n');

            slog::debug!(log, "sending response: {response}");
            writer.write_all(response.as_bytes()).await?;
            msg.clear();
        }

        info!(log, "handle_conn: ALL DONE");
        Ok(())
    }

    pub async fn run(
        log: Logger,
        listener: TcpListener,
        vm_conf_recv: oneshot::Receiver<vm_attest::VmInstanceConf>,
        mut hup_recv: oneshot::Receiver<()>,
        sa_addr: SocketAddrV6,
    ) {
        info!(log, "attestation server running");

        // Attestation requests get to the RoT via sled-agent API endpoints.
        let ox_attest: Box<dyn Attest + Send> =
            Box::new(AttestSledAgent::new(sa_addr, &log));
        let rot =
            Arc::new(Mutex::new(vm_attest::VmInstanceRot::new(ox_attest)));

        let vm_conf = Arc::new(Mutex::new(None));

        let vm_conf_cloned = vm_conf.clone();
        tokio::spawn(async move {
            match vm_conf_recv.await {
                Ok(conf) => {
                    *vm_conf_cloned.lock().unwrap() = Some(conf);
                }
                Err(e) => {
                    panic!("unexpected loss of boot digest thread: {:?}", e);
                }
            }
        });

        loop {
            tokio::select! {
                biased;

                // TODO: do we need this
                _ = &mut hup_recv => {
                    return;
                },

                sock_res = listener.accept() => {
                    info!(log, "new client connected");
                    match sock_res {
                        Ok((sock, _addr)) => {
                            let rot = rot.clone();
                            let log = log.clone();
                            let vm_conf = vm_conf.clone();

                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_conn(log.clone(), rot, vm_conf, sock).await {
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
