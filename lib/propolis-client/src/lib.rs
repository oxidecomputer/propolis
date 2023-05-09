// Copyright 2022 Oxide Computer Company
//! A client for the Propolis hypervisor frontend's server API.
//!
//! It is being experimentally migrated to `progenitor` for auto-generation,
//! which is opt-in at present with crate feature `generated`, and additional
//! compatibility impls and re-exports to approximate the former handmade
//! bindings' module layout with crate feature `generated-migration`.
//!
//! Presently, when built with the `generated` flag, the legacy handmade
//! bindings are available in the `handmade` submodule.

#![cfg_attr(
    feature = "generated",
    doc = "This documentation was built with the `generated` feature **on**."
)]
#![cfg_attr(
    not(feature = "generated"),
    doc = "This documentation was built with the `generated` feature **off**."
)]

pub mod instance_spec;

#[cfg(feature = "generated")]
mod generated;
#[cfg(feature = "generated")]
pub use generated::*;

#[cfg(feature = "generated")]
pub mod handmade;
#[cfg(not(feature = "generated"))]
mod handmade;
#[cfg(not(feature = "generated"))]
pub use handmade::*;

#[cfg(feature = "generated-migration")]
pub use types as api;
#[cfg(feature = "generated-migration")]
mod _compat_impls {
    use super::{generated, handmade};

    impl From<handmade::api::DiskRequest> for generated::types::DiskRequest {
        fn from(req: handmade::api::DiskRequest) -> Self {
            let handmade::api::DiskRequest {
                name,
                slot,
                read_only,
                device,
                volume_construction_request,
            } = req;
            Self {
                name,
                slot: slot.into(),
                read_only,
                device,
                volume_construction_request: volume_construction_request.into(),
            }
        }
    }

    impl From<handmade::api::Slot> for generated::types::Slot {
        fn from(slot: handmade::api::Slot) -> Self {
            Self(slot.0)
        }
    }
}

#[cfg(feature = "generated")]
pub mod support {
    use crate::generated::Client as PropolisClient;
    use crate::handmade::api::InstanceSerialConsoleControlMessage;
    use futures::{SinkExt, StreamExt};
    use slog::Logger;
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio_tungstenite::tungstenite::protocol::Role;
    use tokio_tungstenite::tungstenite::{
        Error as WSError, Message as WSMessage,
    };
    // re-export as an escape hatch for crate-version-matching problems
    use self::tungstenite::http;
    pub use tokio_tungstenite::{tungstenite, WebSocketStream};

    trait SerialConsoleStream: AsyncRead + AsyncWrite + Unpin + Send {}
    impl<T: AsyncRead + AsyncWrite + Unpin + Send> SerialConsoleStream for T {}

    /// This is a trivial abstraction wrapping the websocket connection
    /// returned by [crate::generated::Client::instance_serial], providing
    /// the additional functionality of connecting to the new propolis-server
    /// when an instance is migrated (thus providing the illusion of the
    /// connection being seamlessly maintained through migration)
    pub struct InstanceSerialConsoleHelper {
        ws_stream: WebSocketStream<Box<dyn SerialConsoleStream>>,
        log: Option<Logger>,
    }

    impl InstanceSerialConsoleHelper {
        /// Typical use: Pass the [reqwest::Upgraded] connection to the
        /// /instance/serial channel, i.e. the value returned by
        /// `client.instance_serial().send().await?.into_inner()`.
        pub async fn new<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
            upgraded: T,
            log: Option<Logger>,
        ) -> Self {
            let stream: Box<dyn SerialConsoleStream> = Box::new(upgraded);
            let ws_stream =
                WebSocketStream::from_raw_socket(stream, Role::Client, None)
                    .await;
            Self { ws_stream, log }
        }

        /// Sends the given [WSMessage] to the server.
        /// To send character inputs for the console, send [WSMessage::Binary].
        pub async fn send(&mut self, input: WSMessage) -> Result<(), WSError> {
            self.ws_stream.send(input).await
        }

        /// Receive the next [WSMessage] from the server.
        /// Returns [Option::None] if the connection has been terminated.
        /// - [WSMessage::Binary] are character output from the serial console.
        /// - [WSMessage::Close] is a close frame.
        /// - [WSMessage::Text] contain metadata, i.e. about a migration, which
        ///   this function still returns after connecting to the new server
        ///   in case the application needs to take further action (e.g. log
        ///   an event, or show a UI indicator that a migration has occurred).
        pub async fn recv(
            &mut self,
        ) -> Option<
            Result<WSMessage, WSError>, //Box<dyn std::error::Error + Send + Sync + 'static>>,
        > {
            let value = self.ws_stream.next().await;
            if let Some(Ok(WSMessage::Text(json))) = &value {
                match serde_json::from_str(json) {
                    Ok(InstanceSerialConsoleControlMessage::Migrating {
                        destination,
                        from_start,
                    }) => {
                        let client = PropolisClient::new(&format!(
                            "http://{}",
                            destination
                        ));
                        match client
                            .instance_serial()
                            .from_start(from_start)
                            .send()
                            .await
                        {
                            Ok(resp) => {
                                let stream: Box<dyn SerialConsoleStream> =
                                    Box::new(resp.into_inner());
                                self.ws_stream =
                                    WebSocketStream::from_raw_socket(
                                        stream,
                                        Role::Client,
                                        None,
                                    )
                                    .await;
                            }
                            Err(e) => {
                                return Some(Err(WSError::Http(
                                    http::Response::new(Some(e.to_string())),
                                )))
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(log) = &self.log {
                            slog::warn!(
                                log,
                                "Unsupported control message {:?}: {:?}",
                                json,
                                e
                            );
                        }
                        // don't return error, might be a future addition understood by consumer
                    }
                }
            }
            value.map(|x| x.map_err(Into::into))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::InstanceSerialConsoleControlMessage;
        use super::InstanceSerialConsoleHelper;
        use super::Role;
        use super::WSError;
        use super::WSMessage;
        use super::WebSocketStream;
        use futures::{SinkExt, StreamExt};
        use std::net::SocketAddr;
        #[tokio::test]
        async fn test_connection_helper() {
            let (client_conn, server_conn) = tokio::io::duplex(1024);

            let mut client =
                InstanceSerialConsoleHelper::new(client_conn, None).await;
            let mut server = WebSocketStream::from_raw_socket(
                server_conn,
                Role::Server,
                None,
            )
            .await;

            let sent = WSMessage::Binary(vec![1, 3, 3, 7]);
            client.send(sent.clone()).await.unwrap();
            let received = server.next().await.unwrap().unwrap();
            assert_eq!(sent, received);

            let sent = WSMessage::Binary(vec![2, 4, 6, 8]);
            server.send(sent.clone()).await.unwrap();
            let received = client.recv().await.unwrap().unwrap();
            assert_eq!(sent, received);

            // just check that it *tries* to connect
            let payload = serde_json::to_string(
                &InstanceSerialConsoleControlMessage::Migrating {
                    destination: SocketAddr::V4("0.0.0.0:0".parse().unwrap()),
                    from_start: 0,
                },
            )
            .unwrap();
            let sent = WSMessage::Text(payload);
            server.send(sent).await.unwrap();
            let received = client.recv().await.unwrap().unwrap_err();
            assert!(matches!(received, WSError::Http(_)));
        }
    }
}
