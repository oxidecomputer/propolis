//! Interface for making API requests to propolis.
//! This should be replaced with a client generated from the OpenAPI spec.

use reqwest::Body;
use reqwest::IntoUrl;
use serde::de::DeserializeOwned;
use slog::{info, o, Logger};
use std::net::SocketAddr;
use thiserror::Error;
use uuid::Uuid;

pub mod api;

/// Errors which may be returend from the Propolis Client.
#[derive(Debug, Error)]
pub enum Error {
    #[error("Request failed: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("Bad Status: {0}")]
    Status(u16),
}

/// Client-side connection to propolis.
pub struct Client {
    client: reqwest::Client,
    log: Logger,
    address: SocketAddr,
}

// Sends "request", awaits "response", and returns an error on any
// non-success status code.
//
// TODO: Do we want to handle re-directs?
async fn send_and_check_ok(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, Error> {
    let response = request.send().await.map_err(Error::from)?;

    if !response.status().is_success() {
        return Err(Error::Status(response.status().as_u16()));
    }

    Ok(response)
}

// Sends a "request", awaits "response", and parses the body
// into a deserializable type.
async fn send_and_parse_response<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Result<T, Error> {
    send_and_check_ok(request).await?.json().await.map_err(|e| e.into())
}

impl Client {
    pub fn new(address: SocketAddr, log: Logger) -> Client {
        Client {
            client: reqwest::Client::new(),
            log: log.new(o!("propolis_client address" => address.to_string())),
            address,
        }
    }

    async fn get<T: DeserializeOwned, U: IntoUrl + std::fmt::Display>(
        &self,
        path: U,
        body: Option<Body>,
    ) -> Result<T, Error> {
        info!(self.log, "GET request to {}", path);
        let mut request = self.client.get(path);
        if let Some(body) = body {
            request = request.body(body);
        }

        send_and_parse_response(request).await
    }

    async fn put<T: DeserializeOwned, U: IntoUrl + std::fmt::Display>(
        &self,
        path: U,
        body: Option<Body>,
    ) -> Result<T, Error> {
        info!(self.log, "PUT request to {}", path);
        let mut request = self.client.put(path);
        if let Some(body) = body {
            request = request.body(body);
        }

        send_and_parse_response(request).await
    }

    async fn post<T: DeserializeOwned, U: IntoUrl + std::fmt::Display>(
        &self,
        path: U,
        body: Option<Body>,
    ) -> Result<T, Error> {
        info!(self.log, "POST request to {}", path);
        let mut request = self.client.post(path);
        if let Some(body) = body {
            request = request.body(body);
        }

        send_and_parse_response(request).await
    }

    async fn put_no_response<U: IntoUrl + std::fmt::Display>(
        &self,
        path: U,
        body: Option<Body>,
    ) -> Result<(), Error> {
        info!(self.log, "PUT request to {}", path);
        let mut request = self.client.put(path);
        if let Some(body) = body {
            request = request.body(body);
        }

        send_and_check_ok(request).await?;
        Ok(())
    }

    /// Ensures that an instance with the specified properties exists.
    pub async fn instance_ensure(
        &self,
        request: &api::InstanceEnsureRequest,
    ) -> Result<api::InstanceEnsureResponse, Error> {
        let path = format!("http://{}/instance", self.address,);
        let body = Body::from(serde_json::to_string(&request).unwrap());
        self.put(path, Some(body)).await
    }

    /// Returns information about an instance, by UUID.
    pub async fn instance_get(
        &self,
    ) -> Result<api::InstanceGetResponse, Error> {
        let path = format!("http://{}/instance", self.address);
        self.get(path, None).await
    }

    /// Long-poll for state changes.
    pub async fn instance_state_monitor(
        &self,
        gen: u64,
    ) -> Result<api::InstanceStateMonitorResponse, Error> {
        let path = format!("http://{}/instance/state-monitor", self.address);
        let body = Body::from(
            serde_json::to_string(&api::InstanceStateMonitorRequest { gen })
                .unwrap(),
        );
        self.get(path, Some(body)).await
    }

    /// Puts an instance into a new state.
    pub async fn instance_state_put(
        &self,
        state: api::InstanceStateRequested,
    ) -> Result<(), Error> {
        let path = format!("http://{}/instance/state", self.address);
        let body = Body::from(serde_json::to_string(&state).unwrap());
        self.put_no_response(path, Some(body)).await
    }

    /// Get the status of an ongoing migration
    pub async fn instance_migrate_status(
        &self,
        migration_id: Uuid,
    ) -> Result<api::InstanceMigrateStatusResponse, Error> {
        let path = format!("http://{}/instance/migrate/status", self.address);
        let body = Body::from(
            serde_json::to_string(&api::InstanceMigrateStatusRequest {
                migration_id,
            })
            .unwrap(),
        );
        self.get(path, Some(body)).await
    }

    /// Returns the WebSocket URI to an instance's serial console stream.
    pub fn instance_serial_console_ws_uri(&self) -> String {
        format!("ws://{}/instance/serial", self.address)
    }

    pub async fn instance_issue_crucible_snapshot_request(
        &self,
        inventory_name: String,
        snapshot_name: String,
    ) -> Result<(), Error> {
        let path = format!(
            "http://{}/instance/disk/{}/snapshot/{}",
            self.address, inventory_name, snapshot_name
        );
        self.post(path, None).await
    }
}
