//! Interface for making API requests to propolis.
//! This should be replaced with a client generated from the OpenAPI spec.

use reqwest::Body;
use serde::de::DeserializeOwned;
use slog::{Logger, info, o};
use std::net::SocketAddr;
use thiserror::Error;
use uuid::Uuid;

pub mod api;

/// Errors which may be returend from the Propolis Client.
#[derive(Debug, Error)]
pub enum Error {
    #[error("Request failed: {0}")]
    Reqwest(#[from] reqwest::Error),
}

pub struct Client {
    client: reqwest::Client,
    log: Logger,
    address: SocketAddr,
}

impl Client {
    pub fn new(address: SocketAddr, log: Logger) -> Client {
        Client {
            client: reqwest::Client::new(),
            log: log.new(o!("propolis_client address" => address.to_string())),
            address,
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: String, body: Option<Body>) -> Result<T, Error> {
        info!(self.log, "GET request to {}", path);
        let mut request = self.client.get(path);
        if let Some(body) = body {
            request = request.body(body);
        }
        request.send()
            .await
            .map_err(|e| Error::from(e))?
            .json()
            .await
            .map_err(|e| e.into())
    }

    async fn put<T: DeserializeOwned>(&self, path: String, body: Option<Body>) -> Result<T, Error> {
        info!(self.log, "PUT request to {}", path);
        let mut request = self.client.put(path);
        if let Some(body) = body {
            request = request.body(body);
        }
        request.send()
            .await
            .map_err(|e| Error::from(e))?
            .json()
            .await
            .map_err(|e| e.into())
    }

    pub async fn instance_ensure(
        &self,
        request: &api::InstanceEnsureRequest,
    ) -> Result<api::InstanceEnsureResponse, Error> {
        let path = format!("http://{}/instances/{}", self.address, request.properties.id);
        let body = Body::from(serde_json::to_string(&request).unwrap());
        self.put(path, Some(body)).await
    }

    pub async fn instance_get(
        &self,
        id: Uuid,
    ) -> Result<api::InstanceGetResponse, Error> {
        let path = format!("http://{}/instances/{}", self.address, id);
        self.get(path, None).await
    }

    pub async fn instance_state_put(
        &self,
        id: Uuid,
        state: api::InstanceStateRequested,
    ) -> Result<(), Error> {
        let path = format!("http://{}/instances/{}/state", self.address, id);
        let body = Body::from(serde_json::to_string(&state).unwrap());
        let _ = self.client
            .put(path)
            .body(body)
            .send()
            .await
            .map_err(|e| Error::from(e))?;
        Ok(())
    }

    pub async fn instance_serial(
        &self,
        id: Uuid,
        state: api::InstanceSerialRequest,
    ) -> Result<api::InstanceSerialResponse, Error> {
        let path = format!("http://{}/instances/{}/serial", self.address, id);
        let body = Body::from(serde_json::to_string(&state).unwrap());
        self.put(path, Some(body)).await
    }
}
