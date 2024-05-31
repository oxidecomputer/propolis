// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2022 Oxide Computer Company

use anyhow::{bail, Result};
use async_trait::async_trait;
use clap::{Parser, ValueEnum};
use env_logger;
use image::io::Reader as ImageReader;
use image::GenericImageView;
use log::info;
use rfb::encodings::RawEncoding;
use rfb::rfb::{
    FramebufferUpdate, KeyEvent, PixelFormat, ProtoVersion, Rectangle, SecurityType, SecurityTypes,
};
use rfb::{
    pixel_formats::rgb_888,
    server::{Server, VncServer, VncServerConfig, VncServerData},
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

const WIDTH: usize = 1024;
const HEIGHT: usize = 768;

#[derive(Parser, Debug)]
/// A simple VNC server that displays a single image or color, in a given pixel format
///
/// By default, the server will display the Oxide logo image using little-endian RGBx as its pixel format. To specify an alternate image or color, use the `-i` flag:
/// ./example-server -i test-tubes
/// ./example-server -i red
///
/// To specify an alternate pixel format, use the `--big-endian` flag and/or the ordering flags. The
/// server will transform the input image/color to the requested pixel format and use the format
/// for the RFB protocol.
///
/// For example, to use big-endian xRGB:
/// ./example-server --big-endian true -r 1 -g 2 -b 3
///
struct Args {
    /// Image/color to display from the server
    #[clap(value_enum, short, long, default_value_t = Image::Oxide)]
    image: Image,

    /// Pixel endianness
    #[clap(long, default_value_t = false, action = clap::ArgAction::Set)]
    big_endian: bool,

    /// Byte mapping to red (4-byte RGB pixel, endian-agnostic)
    #[clap(short, long, default_value_t = 0)]
    red_order: u8,

    /// Byte mapping to green (4-byte RGB pixel, endian-agnostic)
    #[clap(short, long, default_value_t = 1)]
    green_order: u8,

    /// Byte mapping to blue (4-byte RGB pixel, endian-agnostic)
    #[clap(short, long, default_value_t = 2)]
    blue_order: u8,
}

#[derive(ValueEnum, Debug, Copy, Clone)]
enum Image {
    Oxide,
    ColorBars,
    Red,
    Green,
    Blue,
    White,
    Black,
}

#[derive(Clone)]
struct ExampleServer {
    display: Image,
    rgb_order: (u8, u8, u8),
    big_endian: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let args = Args::parse();
    validate_order(args.red_order, args.green_order, args.blue_order)?;

    let pf = PixelFormat::new_colorformat(
        rgb_888::BITS_PER_PIXEL,
        rgb_888::DEPTH,
        args.big_endian,
        order_to_shift(args.red_order),
        rgb_888::MAX_VALUE,
        order_to_shift(args.green_order),
        rgb_888::MAX_VALUE,
        order_to_shift(args.blue_order),
        rgb_888::MAX_VALUE,
    );
    info!(
        "Starting server: image: {:?}, pixel format; {:#?}",
        args.image, pf
    );

    let config = VncServerConfig {
        addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 9000),
        version: ProtoVersion::Rfb38,
        sec_types: SecurityTypes(vec![SecurityType::None, SecurityType::VncAuthentication]),
        name: "rfb-example-server".to_string(),
    };
    let data = VncServerData {
        width: WIDTH as u16,
        height: HEIGHT as u16,
        input_pixel_format: pf.clone(),
    };
    let server = ExampleServer {
        display: args.image,
        rgb_order: (args.red_order, args.green_order, args.blue_order),
        big_endian: args.big_endian,
    };
    let s = VncServer::new(server, config, data);
    s.start().await?;

    Ok(())
}

fn validate_order(r: u8, g: u8, b: u8) -> Result<()> {
    if r > 3 || g > 3 || b > 3 {
        bail!("r/g/b must have ordering of 0, 1, 2, or 3");
    }

    if r == g || r == b || g == b {
        bail!("r/g/b must have unique orderings");
    }

    Ok(())
}

fn order_to_shift(order: u8) -> u8 {
    assert!(order <= 3);
    (3 - order) * rgb_888::BITS_PER_COLOR
}

fn order_to_index(order: u8, big_endian: bool) -> u8 {
    assert!(order <= 3);

    if big_endian {
        order
    } else {
        4 - order - 1
    }
}

fn generate_color(index: u8, big_endian: bool) -> Vec<u8> {
    const LEN: usize = WIDTH * HEIGHT * rgb_888::BYTES_PER_PIXEL;
    let mut pixels = vec![0x0u8; LEN];

    let idx = order_to_index(index, big_endian);

    let mut x = 0;
    for i in 0..pixels.len() {
        if x == idx {
            pixels[i] = 0xff;
        }

        if x == 3 {
            x = 0;
        } else {
            x += 1;
        }
    }

    pixels
}

fn generate_image(name: &str, big_endian: bool, rgb_order: (u8, u8, u8)) -> Vec<u8> {
    const LEN: usize = WIDTH * HEIGHT * rgb_888::BYTES_PER_PIXEL;
    let mut pixels = vec![0xffu8; LEN];

    let img = ImageReader::open(name).unwrap().decode().unwrap();

    let (r, g, b) = rgb_order;
    let r_idx = order_to_index(r, big_endian) as usize;
    let g_idx = order_to_index(g, big_endian) as usize;
    let b_idx = order_to_index(b, big_endian) as usize;
    let x_idx = rgb_888::unused_index(r_idx, g_idx, b_idx);

    // Convert the input image pixels to the requested pixel format.
    for (x, y, pixel) in img.pixels() {
        let ux = x as usize;
        let uy = y as usize;

        let y_offset = WIDTH * rgb_888::BYTES_PER_PIXEL;
        let x_offset = ux * rgb_888::BYTES_PER_PIXEL;

        pixels[uy * y_offset + x_offset + r_idx] = pixel[0];
        pixels[uy * y_offset + x_offset + g_idx] = pixel[1];
        pixels[uy * y_offset + x_offset + b_idx] = pixel[2];
        pixels[uy * y_offset + x_offset + x_idx] = pixel[3];
    }

    pixels
}

fn generate_pixels(img: Image, big_endian: bool, rgb_order: (u8, u8, u8)) -> Vec<u8> {
    const LEN: usize = WIDTH * HEIGHT * rgb_888::BYTES_PER_PIXEL;

    let (r, g, b) = rgb_order;

    match img {
        Image::Oxide => generate_image("image-examples/oxide.png", big_endian, rgb_order),
        Image::ColorBars => generate_image("image-examples/color-bars.png", big_endian, rgb_order),
        Image::Red => generate_color(r, big_endian),
        Image::Green => generate_color(g, big_endian),
        Image::Blue => generate_color(b, big_endian),
        Image::White => vec![0xffu8; LEN],
        Image::Black => vec![0x0u8; LEN],
    }
}

#[async_trait]
impl Server for ExampleServer {
    async fn get_framebuffer_update(&self) -> FramebufferUpdate {
        let pixels_width = 1024;
        let pixels_height = 768;
        let pixels = generate_pixels(self.display, self.big_endian, self.rgb_order);
        let r = Rectangle::new(
            0,
            0,
            pixels_width,
            pixels_height,
            Box::new(RawEncoding::new(pixels)),
        );
        FramebufferUpdate::new(vec![r])
    }

    async fn key_event(&self, _ke: KeyEvent) {}
}
