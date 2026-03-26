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

use vm_attest::{VmInstanceAttester, VmInstanceConf};

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

/// This struct manages providing the requisite data for a corresponding `AttestationSock` to
/// become fully functional.
pub struct AttestationSockInit {
    log: slog::Logger,
    vm_conf_send: oneshot::Sender<VmInstanceConf>,
    // kind of terrible: parts of the VM RoT initializer that must be filled in before `run()`.
    // this *could* and probably should be more builder-y and typestateful.
    pub instance_uuid: Option<uuid::Uuid>,
    pub volume_ref: Option<crucible::Volume>,
}

impl AttestationSockInit {
    /// Construct a future that does any remaining work of collecting VM RoT measurements in
    /// support of this VM's attestation server.
    ///
    /// TODO: it is expected this future is simply spawned onto a Tokio runtime. If the VM is torn
    /// down while this future is running, nothing will stop this future from operating? We
    /// probably need to tie in a shutdown signal from the corresponding `AttestationSockInit` to
    /// discover if we should (for example) stop calculating a boot digest midway.
    pub async fn run(mut self) {
        let uuid = self
            .instance_uuid
            .take()
            .expect("AttestationSockInit was provided instance uuid");
        let mut vm_conf = vm_attest::VmInstanceConf { uuid, boot_digest: None };

        if let Some(volume) = self.volume_ref.take() {
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
                    volume, &self.log,
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
            slog::warn!(self.log, "not computing boot disk digest");
        }

        // keep a log reference around to report potential errors after this is taken and dropped
        // in `provide()`.
        let log = self.log.clone();
        let send_res = self.provide(vm_conf);
        if let Err(_) = send_res {
            slog::error!(
                log,
                "attestation server is not listening for its config?"
            );
        }
    }

    pub fn provide(self, conf: VmInstanceConf) -> Result<(), VmInstanceConf> {
        self.vm_conf_send.send(conf)
    }
}

impl AttestationSock {
    pub async fn new(
        log: Logger,
        sa_addr: SocketAddrV6,
    ) -> io::Result<(Self, AttestationSockInit)> {
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
        let attestation_sock = Self { join_hdl, hup_send };
        let attestation_init = AttestationSockInit {
            log: attest_init_log,
            vm_conf_send,
            instance_uuid: None,
            volume_ref: None,
        };
        Ok((attestation_sock, attestation_init))
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
