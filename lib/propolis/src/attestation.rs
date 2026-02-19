// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Attestation server for VM instances.
//!
//! Provides an async TCP server that handles attestation requests from
//! guests using the `vm-attest-proto` wire protocol (newline-delimited
//! JSON). The server is gated on a [`ReadinessGate`]: requests are
//! rejected with an error until a boot disk digest has been supplied.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use slog::{debug, error, info, Logger};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader,
};
use tokio::net::TcpListener;
use vm_attest_proto::{QualifyingData, Response, VmInstanceRot};

/// A one-shot gate that blocks attestation responses until the boot
/// disk digest is available.
///
/// Internally backed by a `tokio::sync::watch` channel. The sender
/// side is held by the component that computes the digest (typically
/// the VM startup path). The receiver side is cloned into the
/// [`AttestationServer`].
pub struct ReadinessGate {
    tx: tokio::sync::watch::Sender<Option<String>>,
    rx: tokio::sync::watch::Receiver<Option<String>>,
}

impl ReadinessGate {
    /// Create a new gate. The digest starts as `None` (not ready).
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(None);
        Self { tx, rx }
    }

    /// Set the boot disk digest, unblocking the attestation server.
    pub fn set(&self, digest: String) {
        // Ignore the error — it just means the receiver was dropped.
        let _ = self.tx.send(Some(digest));
    }

    /// Get a receiver handle that can be cheaply cloned into async
    /// tasks.
    pub fn receiver(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<String>> {
        self.rx.clone()
    }
}

/// Compute the SHA-256 digest of a file (e.g. a boot disk image).
///
/// The file is read in 64 KiB chunks on a blocking thread so that
/// the caller's async runtime is not stalled.
pub async fn compute_disk_digest(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(&path)
            .with_context(|| {
                format!("open disk image {}", path.display())
            })?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).with_context(|| {
                format!("read disk image {}", path.display())
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let hash = hasher.finalize();
        let mut hex = String::with_capacity(hash.len() * 2);
        for byte in hash {
            use std::fmt::Write;
            write!(hex, "{byte:02x}").unwrap();
        }
        Ok(hex)
    })
    .await
    .context("disk digest task panicked")?
}

/// An async TCP attestation server.
///
/// Speaks the `vm-attest-proto` newline-delimited JSON wire protocol:
/// the client sends a JSON-serialized [`QualifyingData`] per line, and
/// the server responds with a JSON-serialized [`Response`] per line.
///
/// The server is generic over the concrete [`VmInstanceRot`]
/// implementation, allowing both mock and IPCC backends.
pub struct AttestationServer<R: VmInstanceRot> {
    log: Logger,
    bind_addr: SocketAddr,
    rot: R,
    gate_rx: tokio::sync::watch::Receiver<Option<String>>,
}

impl<R> AttestationServer<R>
where
    R: VmInstanceRot + Send + Sync + 'static,
{
    /// Create a new server. It will not begin accepting connections
    /// until [`Self::run`] is called.
    pub fn new(
        log: Logger,
        bind_addr: SocketAddr,
        rot: R,
        gate_rx: tokio::sync::watch::Receiver<Option<String>>,
    ) -> Self {
        Self { log, bind_addr, rot, gate_rx }
    }

    /// Run the server loop. This future resolves only on fatal error.
    pub async fn run(self) -> Result<()> {
        let listener =
            TcpListener::bind(self.bind_addr).await.with_context(
                || {
                    format!(
                        "bind attestation server to {}",
                        self.bind_addr
                    )
                },
            )?;

        info!(
            self.log,
            "attestation server listening";
            "addr" => %self.bind_addr,
        );

        loop {
            let (stream, peer) = listener.accept().await?;
            info!(self.log, "attestation client connected";
                "peer" => %peer);

            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();

            loop {
                line.clear();
                let n = reader.read_line(&mut line).await?;
                if n == 0 {
                    debug!(self.log, "client disconnected (EOF)";
                        "peer" => %peer);
                    break;
                }

                debug!(self.log, "received message";
                    "peer" => %peer,
                    "msg" => &line.trim());

                // Check readiness gate before processing.
                if self.gate_rx.borrow().is_none() {
                    let response = Response::Error(
                        "attestation not ready: awaiting boot \
                         disk digest"
                            .to_string(),
                    );
                    let mut resp_json =
                        serde_json::to_string(&response)?;
                    resp_json.push('\n');
                    writer.write_all(resp_json.as_bytes()).await?;
                    continue;
                }

                let qualifying_data: QualifyingData =
                    match serde_json::from_str(line.trim()) {
                        Ok(q) => q,
                        Err(e) => {
                            let response =
                                Response::Error(e.to_string());
                            let mut resp_json =
                                serde_json::to_string(&response)?;
                            resp_json.push('\n');
                            writer
                                .write_all(resp_json.as_bytes())
                                .await?;
                            error!(
                                self.log,
                                "bad qualifying data from client";
                                "peer" => %peer,
                                "error" => %e,
                            );
                            continue;
                        }
                    };

                debug!(self.log, "qualifying data received";
                    "peer" => %peer);

                let response =
                    match self.rot.attest(&qualifying_data) {
                        Ok(a) => Response::Success(a),
                        Err(e) => {
                            Response::Error(format!("{e:?}"))
                        }
                    };

                let mut resp_json =
                    serde_json::to_string(&response)?;
                resp_json.push('\n');

                debug!(self.log, "sending response";
                    "peer" => %peer);
                writer.write_all(resp_json.as_bytes()).await?;
            }
        }
    }
}
