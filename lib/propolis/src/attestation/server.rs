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
use tokio::sync::{oneshot, Mutex as TokioMutex};
use tokio::task::JoinHandle;

use dice_verifier::sled_agent::AttestSledAgent;
use dice_verifier::Attest;

use vm_attest::VmInstanceConf;

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
    log: slog::Logger,
    join_hdl: JoinHandle<()>,
    hup_send: oneshot::Sender<()>,
    init_state: AttestationInitState,
}

#[derive(Debug)]
enum AttestationInitState {
    Preparing {
        vm_conf_send: oneshot::Sender<VmInstanceConf>,
    },
    /// A transient state while we're getting the initializer ready, having
    /// taken `Preparing` and its `vm_conf_send`, but before we've got a
    /// `JoinHandle` to track as running.
    Initializing,
    Running {
        init_task: JoinHandle<()>,
    },
}

/// This struct manages providing the requisite data for a corresponding
/// `AttestationSock` to become fully functional.
pub struct AttestationSockInit {
    log: slog::Logger,
    vm_conf_send: oneshot::Sender<VmInstanceConf>,
    uuid: uuid::Uuid,
    volume_ref: Option<crucible::Volume>,
}

impl AttestationSockInit {
    /// Construct a future that does any remaining work of collecting VM RoT measurements in
    /// support of this VM's attestation server.
    ///
    /// TODO: it is expected this future is simply spawned onto a Tokio runtime. If the VM is torn
    /// down while this future is running, nothing will stop this future from operating? We
    /// probably need to tie in a shutdown signal from the corresponding `AttestationSockInit` to
    /// discover if we should (for example) stop calculating a boot digest midway.
    pub async fn run(self) {
        let AttestationSockInit { log, vm_conf_send, uuid, volume_ref } = self;

        let mut vm_conf = vm_attest::VmInstanceConf { uuid, boot_digest: None };

        if let Some(volume) = volume_ref {
            // TODO: load-bearing sleep: we have a Crucible volume, but we can be here and chomping
            // at the bit to get a digest calculation started well before the volume has been
            // activated; in `propolis-server` we need to wait for at least a subsequent instance
            // start. Similar to the scrub task for Crucible disks, delay some number of seconds in
            // the hopes that activation is done promptly.
            //
            // This should be replaced by awaiting for some kind of actual "activated" signal.
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;

            let boot_digest =
                match crate::attestation::boot_digest::boot_disk_digest(
                    volume, &log,
                )
                .await
                {
                    Ok(digest) => digest,
                    Err(e) => {
                        // a panic here is unfortunate, but helps us debug for now; if the digest
                        // calculation fails it may be some retryable issue that a guest OS would
                        // survive. but panicking here means we've stopped Propolis at the actual
                        // error, rather than noticing the `vm_conf_sender` having dropped
                        // elsewhere.
                        panic!("failed to compute boot disk digest: {e:?}");
                    }
                };

            vm_conf.boot_digest = Some(boot_digest);
        } else {
            slog::warn!(log, "not computing boot disk digest");
        }

        // keep a log reference around to report potential errors after this is taken and dropped
        // in `provide()`.
        let log = log.clone();
        let send_res = vm_conf_send.send(vm_conf);
        if let Err(_) = send_res {
            slog::error!(
                log,
                "attestation server is not listening for its config?"
            );
        }
    }
}

impl AttestationSock {
    pub async fn new(log: Logger, sa_addr: SocketAddrV6) -> io::Result<Self> {
        info!(log, "attestation server created");
        let listener = TcpListener::bind(ATTESTATION_ADDR).await?;
        let (vm_conf_send, vm_conf_recv) =
            oneshot::channel::<vm_attest::VmInstanceConf>();
        let (hup_send, hup_recv) = oneshot::channel::<()>();
        // TODO: would love to describe this sub-log as specifically VM RoT init, but dunno how to
        // drive slog like that.
        let attest_init_log = log.clone();
        let join_hdl = tokio::spawn(async move {
            Self::run(log, listener, vm_conf_recv, hup_recv, sa_addr).await;
        });
        let attestation_sock = Self {
            log: attest_init_log,
            join_hdl,
            hup_send,
            init_state: AttestationInitState::Preparing { vm_conf_send },
        };
        Ok(attestation_sock)
    }

    pub async fn halt(self) {
        let Self { join_hdl, hup_send, init_state, log: _ } = self;

        // Signal the socket listener to hang up, then wait for it to bail
        let _ = hup_send.send(());
        let _ = join_hdl.await;

        if let AttestationInitState::Running { init_task } = init_state {
            init_task.abort();
        }
    }

    // Handle an incoming connection to the attestation port.
    async fn handle_conn(
        log: Logger,
        rot: Arc<TokioMutex<vm_attest::VmInstanceRot>>,
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

                    let conf = {
                        let guard = vm_conf.lock().unwrap();
                        guard.to_owned()
                    };

                    match conf {
                        Some(conf) => {
                            info!(log, "vm conf is ready = {:?}", conf);

                            let rot_guard = rot.lock().await;

                            match rot_guard.attest(&conf, &q).await {
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

    pub fn prepare_instance_conf(
        &mut self,
        uuid: uuid::Uuid,
        volume_ref: Option<crucible::Volume>,
    ) {
        let init_state = std::mem::replace(
            &mut self.init_state,
            AttestationInitState::Initializing,
        );
        let vm_conf_send = match init_state {
            AttestationInitState::Preparing { vm_conf_send } => vm_conf_send,
            other => {
                panic!(
                    "VM RoT used incorrectly: prepare_instance_conf called \
                        more than once. current state {other:?}"
                );
            }
        };
        let init = AttestationSockInit {
            log: self.log.clone(),
            uuid,
            volume_ref,
            vm_conf_send,
        };
        let init_task = tokio::spawn(init.run());
        self.init_state = AttestationInitState::Running { init_task };
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
        let ox_attest: Box<dyn Attest + Send + Sync> =
            Box::new(AttestSledAgent::new(sa_addr, &log));
        let rot =
            Arc::new(TokioMutex::new(vm_attest::VmInstanceRot::new(ox_attest)));

        let vm_conf = Arc::new(Mutex::new(None));

        let log_ref = log.clone();
        let vm_conf_cloned = vm_conf.clone();
        tokio::spawn(async move {
            match vm_conf_recv.await {
                Ok(conf) => {
                    *vm_conf_cloned.lock().unwrap() = Some(conf);
                }
                Err(_e) => {
                    slog::info!(
                        log_ref,
                        "lost of boot digest sender, \
                        hopefully Propolis is stopping"
                    );
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
