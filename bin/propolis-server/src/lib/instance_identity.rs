// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! POC DRAFT — a vsock service that hands a guest an Oxide instance-identity
//! OIDC token (JWT).
//!
//! This is the propolis half of the attestation -> token flow whose Nexus half
//! lives in omicron's lockstep API (`/instance-identity/nonce` and
//! `/instances/{id}/identity-token`).
//!
//! It orchestrates, entirely host-side, so the guest just opens the vsock port
//! and reads back a token:
//!   1. fetch a challenge nonce from Nexus (lockstep API);
//!   2. obtain a `VmInstanceAttestation` from the **local attestation server**
//!      (the existing vsock attestation service on `ATTESTATION_ADDR`, speaking
//!      the `vm_attest` newline-JSON `Request`/`Response` protocol) using that
//!      nonce as the qualifying data — this reuses propolis's existing
//!      attestation production rather than re-implementing it;
//!   3. exchange `{ nonce, attestation }` at Nexus for a signed JWT;
//!   4. return the JWT to the guest.
//!
//! Passing the raw Nexus nonce as the `vm_attest` qualifying data lines up with
//! Nexus's verification, which reconstructs `sha256(OxideInstance log || nonce)`
//! (the `VmInstanceRot` mixes the instance config into the qualifying data the
//! platform RoT signs).
//!
//! These endpoints live on Nexus's **lockstep** API, which is the correct home:
//! propolis is deployed in lockstep with Nexus (`tools/propolis_version`), and
//! the lockstep server is independently resolvable via
//! `ServiceName::NexusLockstep` — a sibling of the `ServiceName::Nexus` propolis
//! already resolves. (The frozen, client-versioned `nexus-internal` API cannot
//! take new endpoints; see omicron#9290.)
//!
//! !!! NOT YET WIRED OR BUILD-VALIDATED. Remaining work to finish this:
//!   - Resolve `nexus_lockstep_url` from `ServiceName::NexusLockstep` via the
//!     same resolver propolis already uses for `ServiceName::Nexus`, instead of
//!     passing a URL in; longer term, use the typed `nexus-lockstep-client`.
//!   - Wire into `initializer.rs`: add a `VsockPortMapping` for `GETTOKEN_PORT`
//!     and spawn `GetTokenServer::run` alongside the attestation server.
//!   - Build + test.

use std::net::SocketAddr;
use std::net::{IpAddr, Ipv4Addr};

use propolis::attestation::ATTESTATION_ADDR;
use serde_json::json;
use slog::{error, info, Logger};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Guest vsock port for the instance-identity token service. 606 sits next to
/// the attestation server's 605.
pub const GETTOKEN_PORT: u16 = 606;
/// Host-side address the vsock proxy forwards `GETTOKEN_PORT` to.
pub const GETTOKEN_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), GETTOKEN_PORT);

/// Max line length accepted on the local attestation socket / from Nexus.
const MAX_LINE_LEN: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum GetTokenError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http error talking to Nexus: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("local attestation server returned an error: {0}")]
    Attestation(String),
    #[error("unexpected response shape from {what}")]
    BadResponse { what: &'static str },
    #[error("nonce was not valid hex/length")]
    BadNonce,
}

/// Configuration for the GetToken server.
#[derive(Clone)]
pub struct GetTokenServerConfig {
    /// Base URL of the Nexus **lockstep** API (e.g. `http://[::1]:12345`).
    /// TODO: resolve this properly; it is *not* the internal-API address.
    pub nexus_lockstep_url: String,
    /// The instance's UUID (used in the token-mint path).
    pub instance_id: uuid::Uuid,
}

pub struct GetTokenServer {
    log: Logger,
    cfg: GetTokenServerConfig,
    http: reqwest::Client,
}

impl GetTokenServer {
    pub fn new(log: Logger, cfg: GetTokenServerConfig) -> Self {
        Self { log, cfg, http: reqwest::Client::new() }
    }

    /// Accept guest connections on `listener` (fed by the vsock proxy) and hand
    /// each one a token.
    pub async fn run(self, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((sock, _)) => {
                    let log = self.log.clone();
                    let cfg = self.cfg.clone();
                    let http = self.http.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_conn(&log, &cfg, &http, sock).await
                        {
                            error!(log, "get-token request failed: {e}");
                        }
                    });
                }
                Err(e) => {
                    error!(log = &self.log, "get-token listener error: {e}");
                }
            }
        }
    }
}

async fn handle_conn(
    log: &Logger,
    cfg: &GetTokenServerConfig,
    http: &reqwest::Client,
    mut guest: TcpStream,
) -> Result<(), GetTokenError> {
    info!(log, "handling get-token request");

    // 1. Nexus-issued challenge nonce.
    let nonce = fetch_nonce(cfg, http).await?;

    // 2. Local attestation over that nonce (reuse the attestation server).
    let attestation = attest_locally(&nonce).await?;

    // 3. Exchange for a token.
    let token = mint_token(cfg, http, &nonce, attestation).await?;

    // 4. Hand it to the guest.
    let mut line = serde_json::to_string(&json!({ "token": token }))?;
    line.push('\n');
    guest.write_all(line.as_bytes()).await?;
    info!(log, "get-token request completed");
    Ok(())
}

async fn fetch_nonce(
    cfg: &GetTokenServerConfig,
    http: &reqwest::Client,
) -> Result<String, GetTokenError> {
    let url = format!("{}/instance-identity/nonce", cfg.nexus_lockstep_url);
    let body: serde_json::Value =
        http.post(url).send().await?.error_for_status()?.json().await?;
    body.get("nonce")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .ok_or(GetTokenError::BadResponse { what: "nonce" })
}

/// Connect to the local attestation server and run the `vm_attest`
/// `Request::Attest` exchange, returning the `VmInstanceAttestation` JSON.
async fn attest_locally(
    nonce_hex: &str,
) -> Result<serde_json::Value, GetTokenError> {
    let nonce: Vec<u8> =
        hex::decode(nonce_hex).map_err(|_| GetTokenError::BadNonce)?;
    if nonce.len() != 32 {
        return Err(GetTokenError::BadNonce);
    }

    // `vm_attest::Request::Attest(QualifyingData([u8;32]))` serializes as
    // `{"Attest":[<32 ints>]}`. Build it directly to avoid a vm_attest dep here.
    let mut req = serde_json::to_string(&json!({ "Attest": nonce }))?;
    req.push('\n');

    let mut sock = TcpStream::connect(ATTESTATION_ADDR).await?;
    sock.write_all(req.as_bytes()).await?;

    let mut reader = BufReader::new(&mut sock).take(MAX_LINE_LEN as u64);
    let mut resp = String::new();
    reader.read_line(&mut resp).await?;

    // Response is `{"Attest": <VmInstanceAttestation>}` or `{"Error": "..."}`.
    let resp: serde_json::Value = serde_json::from_str(&resp)?;
    if let Some(err) = resp.get("Error").and_then(|e| e.as_str()) {
        return Err(GetTokenError::Attestation(err.to_string()));
    }
    resp.get("Attest")
        .cloned()
        .ok_or(GetTokenError::BadResponse { what: "attestation" })
}

async fn mint_token(
    cfg: &GetTokenServerConfig,
    http: &reqwest::Client,
    nonce: &str,
    attestation: serde_json::Value,
) -> Result<String, GetTokenError> {
    let url = format!(
        "{}/instances/{}/identity-token",
        cfg.nexus_lockstep_url, cfg.instance_id
    );
    let body: serde_json::Value = http
        .post(url)
        .json(&json!({ "nonce": nonce, "attestation": attestation }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    body.get("token")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .ok_or(GetTokenError::BadResponse { what: "token" })
}
