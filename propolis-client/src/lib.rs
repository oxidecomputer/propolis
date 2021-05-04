//! Interface for making API requests to propolis.
//! This should be replaced with a client generated from the OpenAPI spec.

use http::Method;
use hyper::Body;
use serde::de::DeserializeOwned;
use slog::Logger;
use std::net::SocketAddr;
use uuid::Uuid;

pub mod api;
mod http_client;

use http_client::{ApiError, HttpClient};

pub struct Client {
    client: HttpClient,
}

impl Client {
    pub fn new(address: SocketAddr, log: Logger) -> Client {
        Client { client: HttpClient::new("propolis", address, log) }
    }

    async fn send_request_await_response<R, P>(
        self,
        path: P,
        method: Method,
        body: Body,
    ) -> Result<R, ApiError>
    where
        R: DeserializeOwned,
        P: AsRef<str>,
    {
        let mut response =
            self.client.request(method.clone(), path.as_ref(), body).await?;

        // TODO-robustness handle 300-level?
        assert!(response.status().is_success());
        let value = self
            .client
            .read_json::<R>(
                &self.client.error_message_base(&method, path.as_ref()),
                &mut response,
            )
            .await?;
        Ok(value)
    }

    pub async fn instance_ensure(
        self,
        properties: &api::InstanceProperties,
    ) -> Result<api::InstanceEnsureResponse, ApiError> {
        let path = format!("/instances/{}", properties.id);
        let body = Body::from(serde_json::to_string(&properties).unwrap());
        self.send_request_await_response(&path, Method::PUT, body).await
    }

    pub async fn instance_get(
        self,
        id: Uuid,
    ) -> Result<api::InstanceGetResponse, ApiError> {
        let path = format!("/instances/{}", id);
        self.send_request_await_response(&path, Method::GET, Body::empty())
            .await
    }

    pub async fn instance_state_put(
        self,
        id: Uuid,
        state: api::InstanceStateRequested,
    ) -> Result<(), ApiError> {
        let path = format!("/instances/{}/state", id);
        let body = Body::from(serde_json::to_string(&state).unwrap());
        self.send_request_await_response(&path, Method::PUT, body).await
    }

    pub async fn instance_serial(
        self,
        id: Uuid,
        state: api::InstanceSerialRequest,
    ) -> Result<api::InstanceSerialResponse, ApiError> {
        let path = format!("/instances/{}/serial", id);
        let body = Body::from(serde_json::to_string(&state).unwrap());
        self.send_request_await_response(&path, Method::PUT, body).await
    }
}
