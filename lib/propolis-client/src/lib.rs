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
    use std::net::SocketAddr;

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

    /// A trait representing a console stream.
    pub trait SerialConsoleStream:
        AsyncRead + AsyncWrite + Unpin + Send
    {
    }
    impl<T: AsyncRead + AsyncWrite + Unpin + Send> SerialConsoleStream for T {}

    /// Represents a way to build a serial console stream.
    #[async_trait::async_trait]
    pub(crate) trait SerialConsoleStreamBuilder {
        async fn build(
            &mut self,
            address: SocketAddr,
            offset: WSClientOffset,
        ) -> Result<Box<dyn SerialConsoleStream>, WSError>;
    }

    /// A serial console builder that uses a Propolis client to build the
    /// socket.
    #[derive(Debug)]
    struct PropolisSerialBuilder {}

    impl PropolisSerialBuilder {
        /// Creates a new `PropolisSerialBuilder`.
        pub fn new() -> Self {
            Self {}
        }
    }

    #[async_trait::async_trait]
    impl SerialConsoleStreamBuilder for PropolisSerialBuilder {
        async fn build(
            &mut self,
            address: SocketAddr,
            offset: WSClientOffset,
        ) -> Result<Box<dyn SerialConsoleStream>, WSError> {
            let client = PropolisClient::new(&format!("http://{}", address));
            let mut req = client.instance_serial();

            match offset {
                WSClientOffset::FromStart(offset) => {
                    req = req.from_start(offset);
                }
                WSClientOffset::MostRecent(offset) => {
                    req = req.most_recent(offset);
                }
            }

            let upgraded = req
                .send()
                .await
                .map_err(|e| {
                    WSError::Http(http::Response::new(Some(e.to_string())))
                })?
                .into_inner();

            Ok(Box::new(upgraded))
        }
    }

    pub enum WSClientOffset {
        FromStart(u64),
        MostRecent(u64),
    }

    /// This is a trivial abstraction wrapping the websocket connection
    /// returned by [crate::generated::Client::instance_serial], providing
    /// the additional functionality of connecting to the new propolis-server
    /// when an instance is migrated (thus providing the illusion of the
    /// connection being seamlessly maintained through migration)
    pub struct InstanceSerialConsoleHelper {
        stream_builder: Box<dyn SerialConsoleStreamBuilder>,
        ws_stream: WebSocketStream<Box<dyn SerialConsoleStream>>,
        log: Option<Logger>,
    }

    impl InstanceSerialConsoleHelper {
        /// Creates a new serial console helper by using a Propolis client to
        /// connect to the provided address and using the given offset.
        ///
        /// Returns an error if the helper failed to connect to the address.
        pub async fn new(
            address: SocketAddr,
            offset: WSClientOffset,
            log: Option<Logger>,
        ) -> Result<Self, WSError> {
            let stream_builder = PropolisSerialBuilder::new();
            Self::new_with_builder(stream_builder, address, offset, log).await
        }

        // Currently used for testing, and not exposed to clients.
        pub(crate) async fn new_with_builder(
            mut stream_builder: impl SerialConsoleStreamBuilder + 'static,
            address: SocketAddr,
            offset: WSClientOffset,
            log: Option<Logger>,
        ) -> Result<Self, WSError> {
            let stream = stream_builder.build(address, offset).await?;
            let ws_stream =
                WebSocketStream::from_raw_socket(stream, Role::Client, None)
                    .await;
            Ok(Self {
                stream_builder: Box::new(stream_builder),
                ws_stream,
                log,
            })
        }

        /// Sends the given [WSMessage] to the server.
        /// To send character inputs for the console, send [WSMessage::Binary].
        pub async fn send(&mut self, input: WSMessage) -> Result<(), WSError> {
            self.ws_stream.send(input).await
        }

        /// Receives the next [WSMessage] from the server, holding it in
        /// abeyance until it is processed.
        ///
        /// Returns [Option::None] if the connection has been terminated.
        ///
        /// # Cancel safety
        ///
        /// This method is cancel-safe and can be used in a `select!` loop
        /// without causing any messages to be dropped. However,
        /// [InstanceSerialConsoleMessage::process] must be awaited to retrieve
        /// the inner [WSMessage], and that portion is not cancel-safe.
        pub async fn recv(
            &mut self,
        ) -> Option<Result<InstanceSerialConsoleMessage<'_>, WSError>> {
            // Note that ws_stream.next() eventually calls tungstenite's
            // read_message. From manual inspection, it looks like read_message
            // is written in a cancel-safe fashion so pending packets are
            // buffered before being written out.
            //
            // We currently assume and don't test that ws_stream.next() is
            // cancel-safe. That would be a good test to add in the future but
            // will require some testing infrastructure to insert delays in the
            // I/O stream manually.
            let message = self.ws_stream.next().await?;
            match message {
                Ok(message) => Some(Ok(InstanceSerialConsoleMessage {
                    helper: self,
                    message,
                })),
                Err(error) => Some(Err(error)),
            }
        }
    }

    /// A [`WSMessage`] that has been received but not processed yet.
    pub struct InstanceSerialConsoleMessage<'a> {
        helper: &'a mut InstanceSerialConsoleHelper,
        message: WSMessage,
    }

    impl<'a> InstanceSerialConsoleMessage<'a> {
        /// Processes this [WSMessage].
        ///
        /// - [WSMessage::Binary] are character output from the serial console.
        /// - [WSMessage::Close] is a close frame.
        /// - [WSMessage::Text] contain metadata, i.e. about a migration, which
        ///   this function still returns after connecting to the new server in
        ///   case the application needs to take further action (e.g. log an
        ///   event, or show a UI indicator that a migration has occurred).
        ///
        /// # Cancel safety
        ///
        /// This method is *not* cancel-safe and should *not* be called directly
        /// in a `select!` loop. If this future is not awaited to completion,
        /// then not only will messages will be dropped, any pending migrations
        /// will not complete.
        ///
        /// Like other non-cancel-safe futures, it is OK to create this future
        /// *once*, then call it in a `select!` loop by pinning it and selecting
        /// over a `&mut` reference to it. An example is shown in [Resuming an
        /// async
        /// operation](https://tokio.rs/tokio/tutorial/select#resuming-an-async-operation).
        ///
        /// # Why this approach?
        ///
        /// There are two general approaches we can take here to deal with
        /// cancel safety:
        ///
        /// 1. Break apart processing into cancel-safe
        ///    [`InstanceSerialConsoleHelper::recv`] and non-cancel-safe (this
        ///    method) sections. This is the approach chosen here.
        /// 2. Make all of [`InstanceSerialConsoleHelper::recv`] cancel-safe.
        ///    This approach was prototyped in [this propolis
        ///    PR](https://github.com/oxidecomputer/propolis/pull/438), but was
        ///    not chosen.
        ///
        /// Why was approach 1 chosen over 2? It comes down to three reasons:
        ///
        /// 1. This approach is significantly simpler to understand and involves
        ///    less state fiddling.
        /// 2. Once we've received a `Migrating` message, the migration is
        ///    actually *done*. From there onwards, connecting to the new server
        ///    should be very quick and it's OK to block on that.
        /// 3. Once we've received a `Migrating` message, we shouldn't be
        ///    sending further messages to the old websocket stream. With
        ///    approach 2, we'd have to do extra work to buffer up those old
        ///    messages, then send them after migration is complete. That isn't
        ///    an issue with approach 1.
        ///
        /// The current implementation does have an issue where if a migration
        /// is happening and we haven't received the `Migrating` message yet,
        /// we'll send messages over the old websocket stream. This can be
        /// addressed in several ways:
        ///
        /// - Maintain a sequence number and a local bounded buffer for
        ///   messages, and include the sequence number in the `Migrating`
        ///   message. Replay messages starting from the sequence number
        ///   afterwards.
        /// - Buffer messages received during migration on the server rather
        ///   than the client.
        pub async fn process(self) -> Result<WSMessage, WSError> {
            if let WSMessage::Text(json) = &self.message {
                match serde_json::from_str(json) {
                    Ok(InstanceSerialConsoleControlMessage::Migrating {
                        destination,
                        from_start,
                    }) => {
                        let stream = self
                            .helper
                            .stream_builder
                            .build(
                                destination,
                                WSClientOffset::FromStart(from_start),
                            )
                            .await?;
                        self.helper.ws_stream =
                            WebSocketStream::from_raw_socket(
                                stream,
                                Role::Client,
                                None,
                            )
                            .await;
                    }
                    Err(e) => {
                        if let Some(log) = &self.helper.log {
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

            Ok(self.message)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::tungstenite::http;
        use super::InstanceSerialConsoleControlMessage;
        use super::InstanceSerialConsoleHelper;
        use super::Role;
        use super::SerialConsoleStream;
        use super::SerialConsoleStreamBuilder;
        use super::WSClientOffset;
        use super::WSError;
        use super::WSMessage;
        use super::WebSocketStream;
        use futures::{SinkExt, StreamExt};
        use std::collections::HashMap;
        use std::net::IpAddr;
        use std::net::Ipv6Addr;
        use std::net::SocketAddr;
        use std::time::Duration;
        use tokio::io::AsyncRead;
        use tokio::io::AsyncWrite;
        use tokio::io::DuplexStream;
        use tokio::time::Instant;

        struct DuplexBuilder {
            client_conns_and_delays:
                HashMap<SocketAddr, (Duration, DuplexStream)>,
        }

        impl DuplexBuilder {
            pub fn new(
                client_conns_and_delays: impl IntoIterator<
                    Item = (SocketAddr, Duration, DuplexStream),
                >,
            ) -> Self {
                Self {
                    client_conns_and_delays: client_conns_and_delays
                        .into_iter()
                        .map(|(address, delay, stream)| {
                            (address, (delay, stream))
                        })
                        .collect(),
                }
            }
        }

        #[async_trait::async_trait]
        impl SerialConsoleStreamBuilder for DuplexBuilder {
            async fn build(
                &mut self,
                address: SocketAddr,
                // offset is currently unused by this builder. Worth testing in
                // the future.
                _offset: WSClientOffset,
            ) -> Result<Box<dyn SerialConsoleStream>, WSError> {
                if let Some((delay, stream)) =
                    self.client_conns_and_delays.remove(&address)
                {
                    tokio::time::sleep(delay).await;
                    Ok(Box::new(stream))
                } else {
                    Err(WSError::Http(http::Response::new(Some(format!(
                        "no duplex connection found for address {address}"
                    )))))
                }
            }
        }

        #[tokio::test]
        async fn test_connection_helper() {
            let address =
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 12000);
            let (client_conn, server_conn) = tokio::io::duplex(1024);
            let stream_builder =
                DuplexBuilder::new([(address, Duration::ZERO, client_conn)]);

            let mut client = InstanceSerialConsoleHelper::new_with_builder(
                stream_builder,
                address,
                WSClientOffset::FromStart(0),
                None,
            )
            .await
            .unwrap();
            let mut server = make_ws_server(server_conn).await;

            let sent = WSMessage::Binary(vec![1, 3, 3, 7]);
            client.send(sent.clone()).await.unwrap();
            let received = server.next().await.unwrap().unwrap();
            assert_eq!(sent, received);

            let sent = WSMessage::Binary(vec![2, 4, 6, 8]);
            server.send(sent.clone()).await.unwrap();
            let received =
                client.recv().await.unwrap().unwrap().process().await.unwrap();
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
            let received = client
                .recv()
                .await
                .unwrap()
                .unwrap()
                .process()
                .await
                .unwrap_err();
            assert!(matches!(received, WSError::Http(_)));
        }

        // start_paused = true means that the durations passed in are used to
        // just provide a total ordering for awaits -- we don't actually wait
        // that long.
        #[tokio::test(start_paused = true)]
        async fn test_recv_cancel_safety() {
            let address_1 =
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 12000);
            let address_2 =
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 14000);

            let (client_conn_1, server_conn_1) = tokio::io::duplex(1024);
            let (client_conn_2, server_conn_2) = tokio::io::duplex(1024);

            let stream_builder = DuplexBuilder::new([
                (address_1, Duration::ZERO, client_conn_1),
                // Add a delay before connecting to client 2 to test cancel safety.
                (address_2, Duration::from_secs(1), client_conn_2),
            ]);

            let mut client = InstanceSerialConsoleHelper::new_with_builder(
                stream_builder,
                address_1,
                WSClientOffset::FromStart(0),
                None,
            )
            .await
            .unwrap();

            let mut server_1 = make_ws_server(server_conn_1).await;
            let mut server_2 = make_ws_server(server_conn_2).await;

            let payload = serde_json::to_string(
                &InstanceSerialConsoleControlMessage::Migrating {
                    destination: address_2,
                    from_start: 0,
                },
            )
            .unwrap();
            let migration_message = WSMessage::Text(payload);

            let expected = vec![
                migration_message.clone(),
                WSMessage::Binary([5, 6, 7, 8].into()),
                WSMessage::Close(None),
            ];

            // Spawn a separate task that feeds values into all the servers with
            // a delay. This means that the recv() future is sometimes cancelled
            // in the select! loop below, so we can test cancel safety.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                server_1.send(migration_message).await.unwrap();

                // This message sent on server 1 is *ignored* because it is sent
                // after the "migrating" message.
                let sent = WSMessage::Binary([1, 2, 3, 4].into());
                server_1.send(sent).await.unwrap();

                tokio::time::sleep(Duration::from_secs(1)).await;
                let sent = WSMessage::Binary([5, 6, 7, 8].into());
                server_2.send(sent).await.unwrap();

                server_2.close(None).await.unwrap();
            });

            let mut received = Vec::new();

            // This sends periodic messages which causes client.recv() to be
            // canceled sometimes.
            let start = Instant::now();
            let mut interval =
                tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    message = client.recv() => {
                        // XXX At the end of client.recv() we should receive
                        // None, but in reality we receive a BrokenPipe message,
                        // why?
                        let message = message.expect("we terminate this loop before receiving None");
                        let message = message
                            .expect("received a message")
                            .process()
                            .await
                            .expect("no migration error occurred");

                        println!("received message: {message:?}");
                        received.push(message.clone());

                        if let WSMessage::Close(_) = message {
                            break;
                        }
                    }
                    _ = interval.tick() => {
                        println!("interval tick, {:?} elapsed", start.elapsed());
                    }
                }
            }

            assert_eq!(received, expected);
        }

        async fn make_ws_server<S>(conn: S) -> WebSocketStream<S>
        where
            S: AsyncRead + AsyncWrite + Unpin,
        {
            WebSocketStream::from_raw_socket(conn, Role::Server, None).await
        }
    }
}
