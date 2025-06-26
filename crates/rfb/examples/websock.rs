// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2022 Oxide Computer Company

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::Result;
use clap::Parser;
use dropshot::{
    channel, ApiDescription, ConfigDropshot, HttpServerStarter, Query,
    RequestContext, WebsocketConnection,
};
use futures::StreamExt;
use slog::info;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_util::codec::FramedRead;

use rfb::proto::{
    ClientMessageDecoder, PixelFormat, ProtoVersion, Resolution, SecurityType,
    SecurityTypes,
};
use rfb::{self, tungstenite::BinaryWs};
use rgb_frame::FourCC;

mod shared;
use shared::{build_logger, ExampleBackend, Image};

const WIDTH: usize = 1024;
const HEIGHT: usize = 768;

#[derive(Parser, Debug)]
/// A simple VNC server that displays a single image or color, in a given pixel format
struct Args {
    /// Image/color to display from the server
    #[clap(value_enum, short, long, default_value_t = Image::Oxide)]
    image: Image,

    /// FourCC for pixel format
    #[clap(long, default_value_t = FourCC::XB24)]
    fourcc: FourCC,
}

struct AppCtx {
    be: ExampleBackend,
    pf: PixelFormat,
}

async fn run_server(
    mut sock: BinaryWs<impl AsyncRead + AsyncWrite + Unpin>,
    be: ExampleBackend,
    input_pf: PixelFormat,
    log: &slog::Logger,
) {
    let init_res = rfb::server::initialize(
        &mut sock,
        rfb::server::InitParams {
            version: ProtoVersion::Rfb38,

            sec_types: SecurityTypes(vec![
                SecurityType::None,
                SecurityType::VncAuthentication,
            ]),

            name: "rfb-ws-example".to_string(),

            resolution: Resolution {
                width: WIDTH as u16,
                height: HEIGHT as u16,
            },
            format: input_pf.clone(),
        },
    )
    .await;

    match init_res {
        Ok(client_init) => {
            slog::debug!(log, "Client initialized {:?}", client_init);
        }
        Err(e) => {
            slog::info!(log, "Error during client init {:?}", e);
            return;
        }
    }

    let mut output_pf = input_pf.clone();
    let mut decoder = FramedRead::new(sock, ClientMessageDecoder::default());
    loop {
        let msg = match decoder.next().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => {
                slog::info!(log, "Error reading client msg: {:?}", e);
                return;
            }
            None => {
                return;
            }
        };
        let sock = decoder.get_mut();

        use rfb::proto::ClientMessage;

        match msg {
            ClientMessage::SetPixelFormat(out_pf) => {
                output_pf = out_pf;
            }
            ClientMessage::FramebufferUpdateRequest(_req) => {
                let fbu = be.generate(WIDTH, HEIGHT, &output_pf).await;

                if let Err(e) = fbu.write_to(sock).await {
                    slog::info!(log, "Error sending FrambufferUpdate: {:?}", e);
                    return;
                }
                if let Err(e) = sock.flush().await {
                    slog::info!(
                        log,
                        "Error flushing after FrambufferUpdate: {:?}",
                        e
                    );
                    return;
                }
            }
            _ => {
                slog::debug!(log, "RX: Client msg {:?}", msg);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let log = build_logger();

    let args = Args::parse();

    let pf = args.fourcc.into();
    let backend = ExampleBackend::new(args.image);
    let app = AppCtx { be: backend, pf };

    // Build a description of the API.
    let mut api = ApiDescription::new();
    api.register(ws_websockify).unwrap();

    // Set up the server.
    let config_dropshot = ConfigDropshot {
        bind_address: SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            3030,
        ),
        ..Default::default()
    };
    let server = HttpServerStarter::new(&config_dropshot, api, app, &log)
        .map_err(|error| format!("failed to create server: {error}"))?
        .start();

    server.await
}

// HTTP API interface

#[channel {
    protocol = WEBSOCKETS,
    path = "/websockify",
}]
async fn ws_websockify(
    rqctx: RequestContext<AppCtx>,
    _qp: Query<()>,
    upgraded: WebsocketConnection,
) -> dropshot::WebsocketChannelResult {
    let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
        upgraded.into_inner(),
        Role::Server,
        None,
    )
    .await;

    info!(rqctx.log, "New connection from {}", rqctx.request.remote_addr());
    let be = rqctx.server.private.be.clone();
    let pf = rqctx.server.private.pf.clone();
    run_server(BinaryWs::new(ws), be, pf, &rqctx.log).await;

    Ok(())
}
