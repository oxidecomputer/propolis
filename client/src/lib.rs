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
        let path = format!(
            "http://{}/instances/{}",
            self.address, request.properties.id
        );
        let body = Body::from(serde_json::to_string(&request).unwrap());
        self.put(path, Some(body)).await
    }

    /// Returns information about an instance, by UUID.
    pub async fn instance_get(
        &self,
        id: Uuid,
    ) -> Result<api::InstanceGetResponse, Error> {
        let path = format!("http://{}/instances/{}", self.address, id);
        self.get(path, None).await
    }

    /// Gets instance UUID, by name.
    pub async fn instance_get_uuid(&self, name: &str) -> Result<Uuid, Error> {
        let path = format!("http://{}/instances/{}/uuid", self.address, name);
        self.get(path, None).await
    }

    /// Long-poll for state changes.
    pub async fn instance_state_monitor(
        &self,
        id: Uuid,
        gen: u64,
    ) -> Result<api::InstanceStateMonitorResponse, Error> {
        let path =
            format!("http://{}/instances/{}/state-monitor", self.address, id);
        let body = Body::from(
            serde_json::to_string(&api::InstanceStateMonitorRequest { gen })
                .unwrap(),
        );
        self.get(path, Some(body)).await
    }

    /// Puts an instance into a new state.
    pub async fn instance_state_put(
        &self,
        id: Uuid,
        state: api::InstanceStateRequested,
    ) -> Result<(), Error> {
        let path = format!("http://{}/instances/{}/state", self.address, id);
        let body = Body::from(serde_json::to_string(&state).unwrap());
        self.put_no_response(path, Some(body)).await
    }

    /// Get the status of an ongoing migration
    pub async fn instance_migrate_status(
        &self,
        id: Uuid,
        migration_id: Uuid,
    ) -> Result<api::InstanceMigrateStatusResponse, Error> {
        let path =
            format!("http://{}/instances/{}/migrate/status", self.address, id);
        let body = Body::from(
            serde_json::to_string(&api::InstanceMigrateStatusRequest {
                migration_id,
            })
            .unwrap(),
        );
        self.get(path, Some(body)).await
    }
}
