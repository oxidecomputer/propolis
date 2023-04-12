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
    use reqwest::Upgraded;
    use tokio_tungstenite::tungstenite::protocol::Role;
    use tokio_tungstenite::tungstenite::{Error as WSError, Message};
    use tokio_tungstenite::WebSocketStream;

    /// This is a trivial abstraction wrapping the websocket connection
    /// returned by [crate::generated::Client::instance_serial], providing
    /// the additional functionality of connecting to the new propolis-server
    /// when an instance is migrated (thus providing the illusion of the
    /// connection being seamlessly maintained through migration)
    pub struct InstanceSerialConsoleHelper {
        ws_stream: WebSocketStream<Upgraded>,
    }

    impl InstanceSerialConsoleHelper {
        /// Pass the [Upgraded] connection to the /instance/serial channel,
        /// the value of `client.instance_serial().send().await?.into_inner()`.
        pub async fn new(upgraded: Upgraded) -> Self {
            let ws_stream =
                WebSocketStream::from_raw_socket(upgraded, Role::Client, None)
                    .await;
            Self { ws_stream }
        }

        /// Sends the given [Message] to the server.
        /// To send character inputs for the console, send [Message::Binary].
        pub async fn send(&mut self, input: Message) -> Result<(), WSError> {
            self.ws_stream.send(input).await
        }

        /// Receive the next [Message] from the server.
        /// Returns [Option::None] if the connection has been terminated.
        /// - [Message::Binary] are character output from the serial console.
        /// - [Message::Close] is a close frame.
        /// - [Message::Text] contain metadata, i.e. about a migration, which
        ///   this function still returns after connecting to the new server
        ///   in case the application needs to take further action (e.g. log
        ///   an event, or show a UI indicator that a migration has occurred).
        pub async fn recv(
            &mut self,
        ) -> Option<
            Result<Message, Box<dyn std::error::Error + Send + Sync + 'static>>,
        > {
            let value = self.ws_stream.next().await;
            if let Some(Ok(Message::Text(json))) = &value {
                match serde_json::from_str(&json) {
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
                                self.ws_stream =
                                    WebSocketStream::from_raw_socket(
                                        resp.into_inner(),
                                        Role::Client,
                                        None,
                                    )
                                    .await;
                            }
                            Err(e) => return Some(Err(e.to_string().into())),
                        }
                    }
                    Err(e) => return Some(Err(e.into())),
                }
            }
            value.map(|x| x.map_err(Into::into))
        }
    }
}
