// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Context;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;
use std::sync::Mutex;

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

/// TODO: block comment
///
/// - explain vm conf structure: this defines additional data tying the guest challenge (qualifying
/// data) to the instance. Currently it's definition is in the vm_attest crate.
///

// See: https://github.com/oxidecomputer/oana
pub const ATTESTATION_PORT: u16 = 605;
pub const ATTESTATION_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), ATTESTATION_PORT);

pub struct AttestationSock {
    join_hdl: JoinHandle<()>,
    hup_send: oneshot::Sender<()>,
}

impl AttestationSock {
    pub async fn new(log: Logger, sa_addr: SocketAddrV6) -> io::Result<Self> {
        info!(log, "attestation server created");
        let listener = TcpListener::bind(ATTESTATION_ADDR).await?;
        let (vm_conf_send, vm_conf_recv) = oneshot::channel::<vm_attest::VmInstanceConf>();
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

    async fn handle_conn(
        log: Logger,
        rot: Arc<Mutex<VmInstanceRot>>,
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
                let response = Response::Error("Request too long".to_string());
                let mut response = serde_json::to_string(&response)?;
                response.push('\n');
                slog::info!(log, "sending error response: {response}");
                writer.write_all(response.as_bytes()).await?;
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
                    writer.write_all(response.as_bytes()).await?;
                    break;
                }
            };

            let response = match request {
                Request::Attest(q) => {
                    slog::debug!(log, "qualifying data received: {q:?}");

                    let guard = vm_conf.lock().unwrap();
                    let conf = guard.clone();
                    drop(guard);

                    match conf {
                        Some(conf) => {
                            info!(log, "vm conf is ready = {:?}", conf);

                    let rot_guard = rot.lock().unwrap();

                    // very unfortunate: `attest` is a trait of synchronous
                    // functions, in our case wrapping async calls into
                    // sled-agent. to make this work, the sled-agent attestor
                    // holds a runtime handle and will block on async calls
                    // internally. This all happens in `attest()`. We're calling
                    // that from an async task, so just directly calling
                    // `attest()` is a sure-fire way to panic as the contained
                    // runtime tries to start on this already-in-a-runtime
                    // thread.
                    //
                    // okay. so, it'd be ideal to just give AttestSledAgent our
                    // runtime and let it `block_on()`. for now, instead, just
                    // call it in a context it's allowed to do its own
                    // `block_on()`:
                    let attest_result =
                        tokio::task::block_in_place(move || {
                            rot_guard.attest(&conf, &q)
                        });


                            match attest_result {
                                Ok(a) => Response::Attest(a),
                                Err(e) => {
                                    slog::warn!(log, "attestation error: {e:?}");
                                    Response::Error(e.to_string())
                                },
                            }
                        },

                        // The VM conf isn't ready yet.
                        None => {
                            info!(log, "vm conf is NOT ready");
                            let response = Response::Error(
                                "VmInstanceConf not ready".to_string(),
                            );
                            //let mut response =
                                //serde_json::to_string(&response)?;
                            //response.push('\n');
                            response
                        },
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
        let rot = Arc::new(Mutex::new(VmInstanceRot::new(ox_attest)));

        let vm_conf = Arc::new(Mutex::new(None));

        let vm_conf_cloned = vm_conf.clone();
        tokio::spawn(async move {
            match vm_conf_recv.await {
                Ok(conf) => {
                    *vm_conf_cloned.lock().unwrap() = Some(conf);
                },
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
