// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2024 Oxide Computer Company

#![allow(dead_code)]

use std::io::{BufReader, Cursor};

use clap::ValueEnum;
use image::io::Reader as ImageReader;
use rgb_frame::*;
use slog::Drain;

use rfb::encodings::RawEncoding;
use rfb::proto::{
    FramebufferUpdate, PixelFormat, Position, Rectangle, Resolution,
};

const IMG_OXIDE: &[u8] = include_bytes!("images/oxide.png");
const IMG_COLORBARS: &[u8] = include_bytes!("images/color-bars.png");

#[derive(ValueEnum, Debug, Copy, Clone)]
pub enum Image {
    Oxide,
    ColorBars,
    Red,
    Green,
    Blue,
    White,
    Black,
}
#[derive(Clone)]
pub struct ExampleBackend(Image);
impl ExampleBackend {
    pub fn new(img: Image) -> Self {
        Self(img)
    }
    pub async fn generate(
        &self,
        width: usize,
        height: usize,
        format: &PixelFormat,
    ) -> FramebufferUpdate {
        let size = Size { width, height };
        let mut frame = generate_frame(size, self.0);

        if let Ok(fourcc) = format.try_into() {
            frame.convert(fourcc);
        }

        let r = Rectangle {
            position: Position { x: 0, y: 0 },
            dimensions: Resolution {
                width: width as u16,
                height: height as u16,
            },
            data: Box::new(RawEncoding::new(frame.bytes().to_vec())),
        };
        FramebufferUpdate(vec![r])
    }
}

#[derive(Copy, Clone)]
struct Size {
    width: usize,
    height: usize,
}
impl Size {
    const fn len(&self, bytes_per_pixel: usize) -> usize {
        self.width * self.height * bytes_per_pixel
    }
}

fn generate_image(size: Size, img_bytes: &[u8]) -> Frame {
    let image = ImageReader::new(BufReader::new(Cursor::new(img_bytes)))
        .with_guessed_format()
        .unwrap()
        .decode()
        .unwrap()
        .into_rgba8();

    Frame::new_uninit(
        Spec::new(size.width, size.height, FourCC::AB24),
        |data, stride| {
            for y in 0..size.height {
                for x in 0..size.width {
                    let pix = match image.get_pixel_checked(x as u32, y as u32)
                    {
                        Some(px) => px.0,
                        // black, opaque
                        None => [0, 0, 0, 0xff],
                    };

                    let idx = y * stride.get() + x * 4;
                    data[idx].write(pix[0]);
                    data[idx + 1].write(pix[1]);
                    data[idx + 2].write(pix[2]);
                    data[idx + 3].write(pix[3]);
                }
            }
        },
    )
}

fn generate_solid(size: Size, rgb_pixel: [u8; 3]) -> Frame {
    Frame::new_uninit(
        Spec::new(size.width, size.height, FourCC::BA24),
        |data, stride| {
            for y in 0..size.height {
                for x in 0..size.width {
                    let idx = y * stride.get() + x * 4;
                    data[idx].write(rgb_pixel[0]);
                    data[idx + 1].write(rgb_pixel[1]);
                    data[idx + 2].write(rgb_pixel[2]);
                    data[idx + 3].write(0xff);
                }
            }
        },
    )
}

fn generate_frame(size: Size, img: Image) -> Frame {
    match img {
        Image::Oxide => generate_image(size, IMG_OXIDE),
        Image::ColorBars => generate_image(size, IMG_COLORBARS),
        Image::Red => generate_solid(size, [255, 0, 0]),
        Image::Green => generate_solid(size, [0, 255, 0]),
        Image::Blue => generate_solid(size, [0, 0, 255]),
        Image::White => generate_solid(size, [255, 255, 255]),
        Image::Black => generate_solid(size, [0, 0, 0]),
    }
}

pub fn build_logger() -> slog::Logger {
    slog::Logger::root(
        std::sync::Mutex::new(
            slog_envlogger::EnvLogger::new(
                slog_term::FullFormat::new(
                    slog_term::TermDecorator::new().build(),
                )
                .build()
                .fuse(),
            )
            .fuse(),
        )
        .fuse(),
        slog::o!(),
    )
}
