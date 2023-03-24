use anyhow::Result;
use futures::{SinkExt, StreamExt};
use reqwest::Upgraded;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{error, info, info_span, Instrument};

use self::vt100::Vt100Processor;

mod vt100;

pub struct SerialConsole {
    ws_task: JoinHandle<()>,
    vt_processor: vt100::Vt100Processor,
    wstx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

async fn websocket_handler(
    mut ws: WebSocketStream<Upgraded>,
    output_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    mut input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    loop {
        tokio::select! {
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        if output_tx.send(bytes).await.is_err() {
                            break;
                        }
                    },
                    Some(Ok(Message::Close(..))) => {
                        info!("Serial socket closed");
                        break;
                    },
                    Some(Ok(Message::Text(s))) => {
                        info!("Serial socket received control message: {:?}", s);
                    }
                    None => {
                        info!("Serial socket returned None");
                        break;
                    }
                    _ => continue,
                }
            }
            input = input_rx.recv() => {
                match input {
                    Some(c) => {
                        if let Err(e) = ws.send(Message::Binary(c)).await {
                            error!(?e, "Failed to send input to serial socket");
                            break;
                        }
                    },
                    None => {
                        info!("Input channel to serial socket was closed");
                        break;
                    }
                }
            }
        }
    }
}

impl SerialConsole {
    pub async fn new(serial_conn: Upgraded) -> anyhow::Result<Self> {
        let ws =
            WebSocketStream::from_raw_socket(serial_conn, Role::Client, None)
                .await;

        let (vttx, vtrx) = tokio::sync::mpsc::channel(16);
        let (wstx, wsrx) = tokio::sync::mpsc::channel(16);

        let ws_span = info_span!("Serial websocket task");
        ws_span.follows_from(tracing::Span::current());
        let ws_task = tokio::spawn(
            async move { websocket_handler(ws, vttx, wsrx).await }
                .instrument(ws_span),
        );

        Ok(Self { ws_task, vt_processor: Vt100Processor::new(vtrx), wstx })
    }

    pub async fn register_wait_for_string(
        &self,
        wanted: String,
        preceding_tx: mpsc::Sender<String>,
    ) -> Result<()> {
        self.vt_processor
            .register_wait_for_string(wanted, preceding_tx)
            .await?;
        Ok(())
    }

    pub async fn cancel_wait_for_string(&self) {
        self.vt_processor.cancel_wait_for_string().await;
    }

    pub async fn send_bytes(&self, bytes: Vec<u8>) -> Result<()> {
        self.wstx.send(bytes).await?;
        Ok(())
    }
}

impl Drop for SerialConsole {
    fn drop(&mut self) {
        self.ws_task.abort();
    }
}
