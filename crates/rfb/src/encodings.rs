// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2022 Oxide Computer Company

use crate::{
    pixel_formats::rgb_888,
    rfb::{PixelFormat, Position, Resolution},
};

use EncodingType::*;

#[derive(Debug)]
#[allow(unused)]
pub enum EncodingType {
    Raw,
    CopyRect,
    RRE,
    Hextile,
    TRLE,
    ZRLE,
    CursorPseudo,
    DesktopSizePseudo,
    JRLE,
    ZRLE2,
    JPEG,
    Zlib,
    CursorWithAlpha,
    Other(i32),
}

pub trait Encoding
where
    Self: Send,
{
    fn get_type(&self) -> EncodingType;

    /// Transform this encoding from its representation into a byte vector that can be passed to the client.
    fn encode(&self) -> &Vec<u8>;

    /// Translates this encoding type from an input pixel format to an output format.
    fn transform(&self, input: &PixelFormat, output: &PixelFormat) -> Box<dyn Encoding>;
}

impl From<EncodingType> for i32 {
    fn from(e: EncodingType) -> Self {
        match e {
            Raw => 0,
            CopyRect => 1,
            RRE => 2,
            Hextile => 5,
            TRLE => 15,
            ZRLE => 16,
            CursorPseudo => -239,
            DesktopSizePseudo => -223,
            JRLE => 22,
            ZRLE2 => 24,
            JPEG => 21,
            Zlib => 6,
            CursorWithAlpha => -314,
            Other(n) => n,
        }
    }
}

impl From<i32> for EncodingType {
    fn from(value: i32) -> Self {
        match value {
            0 => Raw,
            1 => CopyRect,
            2 => RRE,
            5 => Hextile,
            15 => TRLE,
            16 => ZRLE,
            -239 => CursorPseudo,
            -223 => DesktopSizePseudo,
            22 => JRLE,
            24 => ZRLE2,
            21 => JPEG,
            6 => Zlib,
            -314 => CursorWithAlpha,
            v => EncodingType::Other(v),
        }
    }
}

/// Section 7.7.1
pub struct RawEncoding {
    pixels: Vec<u8>,
}

impl RawEncoding {
    pub fn new(pixels: Vec<u8>) -> Self {
        Self { pixels }
    }
}

impl Encoding for RawEncoding {
    fn get_type(&self) -> EncodingType {
        EncodingType::Raw
    }

    fn encode(&self) -> &Vec<u8> {
        &self.pixels
    }

    fn transform(&self, input: &PixelFormat, output: &PixelFormat) -> Box<dyn Encoding> {
        // XXX: This assumes the pixel formats are both rgb888. The server code verifies this
        // before calling.
        assert!(input.is_rgb_888());
        assert!(output.is_rgb_888());

        Box::new(Self {
            pixels: rgb_888::transform(&self.pixels, &input, &output),
        })
    }
}

#[allow(dead_code)]
struct RREncoding {
    background_pixel: Pixel,
    sub_rectangles: Vec<RRESubrectangle>,
}

#[allow(dead_code)]
struct Pixel {
    bytes: Vec<u8>,
}

#[allow(dead_code)]
struct RRESubrectangle {
    pixel: Pixel,
    position: Position,
    dimensions: Resolution,
}

#[allow(dead_code)]
struct HextileEncoding {
    tiles: Vec<Vec<HextileTile>>,
}

#[allow(dead_code)]
enum HextileTile {
    Raw(Vec<u8>),
    Encoded(HextileTileEncoded),
}

#[allow(dead_code)]
struct HextileTileEncoded {
    background: Option<Pixel>,
    foreground: Option<Pixel>,
    // TODO: finish this
}
