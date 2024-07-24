// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2022 Oxide Computer Company

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::Result;
use clap::Parser;
use futures::StreamExt;
use slog::info;
use tokio::net::TcpListener;
use tokio_util::codec::FramedRead;

use rfb::proto::{
    ClientMessageDecoder, PixelFormat, ProtoVersion, Resolution, SecurityType,
    SecurityTypes,
};
use rgb_frame::FourCC;

mod shared;
use shared::{ExampleBackend, Image};

const WIDTH: usize = 1024;
const HEIGHT: usize = 768;

#[derive(Parser, Debug)]
/// A simple VNC server that displays a single image or color, in a given pixel format
///
/// By default, the server will display the Oxide logo image using little-endian
/// xBGR as its pixel format.
///
/// To specify an alternate image or color, use the `-i` flag:
/// ./example-server -i colorbars
/// ./example-server -i red
///
/// To specify an alternate pixel format, use the --fourcc flag. The server will
/// transform the input image/color to the pixel format corresponding to the
/// specified fourcc and use the format for the RFB protocol.
///
/// For example, to use big-endian xRGB:
/// ./example-server --fourcc XR24
///
struct Args {
    /// Image/color to display from the server
    #[clap(value_enum, short, long, default_value_t = Image::Oxide)]
    image: Image,

    /// FourCC for pixel format
    #[clap(long, default_value_t = FourCC::XB24)]
    fourcc: FourCC,
}

#[tokio::main]
async fn main() -> Result<()> {
    let log = shared::build_logger();

    let args = Args::parse();

    let pf: PixelFormat = args.fourcc.into();
    info!(
        log,
        "Starting server: image: {:?}, pixel format; {:#?}", args.image, pf
    );

    let backend = ExampleBackend::new(args.image);

    let listener = TcpListener::bind(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        9000,
    ))
    .await
    .unwrap();

    loop {
        let (mut sock, addr) = listener.accept().await.unwrap();

        info!(log, "New connection from {:?}", addr);
        let log_child = log.new(slog::o!("sock" => addr));

        let init_res = rfb::server::initialize(
            &mut sock,
            rfb::server::InitParams {
                version: ProtoVersion::Rfb38,

                sec_types: SecurityTypes(vec![
                    SecurityType::None,
                    SecurityType::VncAuthentication,
                ]),

                name: "rfb-example-server".to_string(),

                resolution: Resolution {
                    width: WIDTH as u16,
                    height: HEIGHT as u16,
                },
                format: pf.clone(),
            },
        )
        .await;

        if let Err(e) = init_res {
            slog::info!(log_child, "Error during client init {:?}", e);
            continue;
        }

        let be_clone = backend.clone();
        let input_pf = pf.clone();
        tokio::spawn(async move {
            let mut output_pf = input_pf.clone();
            let mut decoder =
                FramedRead::new(sock, ClientMessageDecoder::default());
            loop {
                let msg = match decoder.next().await {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        slog::info!(
                            log_child,
                            "Error reading client msg: {:?}",
                            e
                        );
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
                        let fbu =
                            be_clone.generate(WIDTH, HEIGHT, &output_pf).await;

                        if let Err(e) = fbu.write_to(sock).await {
                            slog::info!(
                                log_child,
                                "Error sending FrambufferUpdate: {:?}",
                                e
                            );
                            return;
                        }
                    }
                    _ => {
                        slog::debug!(log_child, "RX: Client msg {:?}", msg);
                    }
                }
            }
        });
    }
}
