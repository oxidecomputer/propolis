// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified VM-instance RoT vsock service.
//!
//! A single vsock service that routes on `vm_attest::Request`:
//!   - `Attest(qualifying_data)` -> produce a `VmInstanceAttestation`
//!     (`Response::Attest`), exactly as the standalone attestation server did;
//!   - `GetToken` -> mint an Oxide instance-identity OIDC token
//!     (`Response::Token`) by orchestrating, host-side:
//!       1. fetch a challenge nonce from Nexus's lockstep API,
//!       2. produce an attestation over that nonce (the platform RoT signs
//!          `sha256(VmInstanceConf || nonce)`),
//!       3. exchange `{ nonce, attestation }` at Nexus for a signed JWT.
//!
//! Collapsing both into one server (one vsock port, message-routed) avoids a
//! second port and the previous draft's internal `localhost:605` hop. It lives
//! in propolis-server (not `lib/propolis`) because `GetToken` calls Nexus over
//! HTTP (`reqwest`), while still holding a `VmInstanceRot` for attestation
//! production. The endpoints are on Nexus's *lockstep* API, resolvable via
//! `ServiceName::NexusLockstep`.
//!
//! !!! NOT YET WIRED INTO initializer.rs / NOT BUILD-VALIDATED. Remaining:
//!   - initializer: resolve `ServiceName::NexusLockstep`, construct this server,
//!     add a single `VsockPortMapping`, and spawn it *instead of* the
//!     `lib/propolis` `AttestationSock` (which this supersedes);
//!   - the POC omits the boot-disk digest (`VmInstanceConf.boot_digest = None`),
//!     which the old `AttestationSock` computed -- re-add if needed;
//!   - build + test.

use std::net::SocketAddrV6;
use std::sync::Arc;

use dice_verifier::sled_agent::AttestSledAgent;
use dice_verifier::Attest;
use slog::{error, info, Logger};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex as TokioMutex;

use vm_attest::{
    QualifyingData, Request, Response, VmInstanceAttestation, VmInstanceConf,
    VmInstanceRot,
};

const MAX_LINE_LEN: usize = 64 * 1024;

pub struct InstanceIdentityServer {
    log: Logger,
    rot: TokioMutex<VmInstanceRot>,
    vm_conf: VmInstanceConf,
    http: reqwest::Client,
    /// Base URL of the Nexus **lockstep** API (e.g. `http://[::1]:port`),
    /// resolved from `ServiceName::NexusLockstep`.
    nexus_lockstep_url: String,
}

impl InstanceIdentityServer {
    pub fn new(
        log: Logger,
        sled_agent_addr: SocketAddrV6,
        instance_id: uuid::Uuid,
        nexus_lockstep_url: String,
    ) -> Self {
        // Attestation requests reach the RoT via sled-agent (same path the old
        // attestation server used).
        let attest: Box<dyn Attest + Send + Sync> =
            Box::new(AttestSledAgent::new(sled_agent_addr, &log));
        let rot = TokioMutex::new(VmInstanceRot::new(attest));
        // POC: boot digest omitted.
        let vm_conf =
            VmInstanceConf { uuid: instance_id, boot_digest: None };
        Self {
            log,
            rot,
            vm_conf,
            http: reqwest::Client::new(),
            nexus_lockstep_url,
        }
    }

    /// Accept guest connections (fed by the vsock proxy) and service each one.
    pub async fn run(self, listener: TcpListener) {
        let me = Arc::new(self);
        loop {
            match listener.accept().await {
                Ok((sock, _)) => {
                    let me = Arc::clone(&me);
                    tokio::spawn(async move {
                        if let Err(e) = me.handle_conn(sock).await {
                            error!(
                                me.log,
                                "instance-identity request failed: {e}"
                            );
                        }
                    });
                }
                Err(e) => {
                    error!(me.log, "listener error: {e}");
                }
            }
        }
    }

    async fn handle_conn(
        &self,
        mut sock: TcpStream,
    ) -> std::io::Result<()> {
        info!(self.log, "handling instance-identity request");

        let mut line = String::new();
        {
            let mut reader =
                BufReader::new(&mut sock).take(MAX_LINE_LEN as u64);
            reader.read_line(&mut line).await?;
        }

        let response = match serde_json::from_str::<Request>(line.trim()) {
            Ok(Request::Attest(q)) => self.do_attest(&q).await,
            Ok(Request::GetToken) => self.do_get_token().await,
            Err(e) => Response::Error(format!("bad request: {e}")),
        };

        let mut out = serde_json::to_string(&response).unwrap_or_else(|e| {
            format!("{{\"Error\":\"failed to serialize response: {e}\"}}")
        });
        out.push('\n');
        sock.write_all(out.as_bytes()).await?;
        Ok(())
    }

    async fn do_attest(&self, q: &QualifyingData) -> Response {
        let rot = self.rot.lock().await;
        match rot.attest(&self.vm_conf, q).await {
            Ok(att) => Response::Attest(att),
            Err(e) => Response::Error(e.to_string()),
        }
    }

    async fn do_get_token(&self) -> Response {
        match self.get_token_inner().await {
            Ok(jwt) => Response::Token(jwt),
            Err(e) => Response::Error(e),
        }
    }

    async fn get_token_inner(&self) -> Result<String, String> {
        // 1. Nexus-issued challenge nonce.
        let nonce_hex =
            self.fetch_nonce().await.map_err(|e| e.to_string())?;
        let nonce: [u8; 32] = hex::decode(&nonce_hex)
            .map_err(|e| e.to_string())?
            .try_into()
            .map_err(|_| "nonce is not 32 bytes".to_string())?;

        // 2. Attestation over that nonce (reusing the same RoT path as Attest).
        let attestation = {
            let rot = self.rot.lock().await;
            rot.attest(&self.vm_conf, &QualifyingData::from(nonce))
                .await
                .map_err(|e| e.to_string())?
        };

        // 3. Exchange for a token.
        self.mint_token(&nonce_hex, &attestation)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_nonce(&self) -> Result<String, reqwest::Error> {
        let url =
            format!("{}/instance-identity/nonce", self.nexus_lockstep_url);
        let body: serde_json::Value =
            self.http.post(url).send().await?.error_for_status()?.json().await?;
        Ok(body
            .get("nonce")
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .to_string())
    }

    async fn mint_token(
        &self,
        nonce: &str,
        attestation: &VmInstanceAttestation,
    ) -> Result<String, reqwest::Error> {
        let url = format!(
            "{}/instances/{}/identity-token",
            self.nexus_lockstep_url, self.vm_conf.uuid
        );
        let body: serde_json::Value = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "nonce": nonce,
                "attestation": attestation,
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(body
            .get("token")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string())
    }
}
