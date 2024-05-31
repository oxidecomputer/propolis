// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2022 Oxide Computer Company

use crate::proto::{Position, Resolution};

use strum::FromRepr;

#[derive(Debug, FromRepr, Ord, PartialOrd, Eq, PartialEq)]
#[repr(i32)]
pub enum EncodingType {
    Raw = 0,
    CopyRect = 1,
    RRE = 2,
    CoRRE = 4,
    Hextile = 5,
    Zlib = 6,
    TRLE = 15,
    ZRLE = 16,
    JPEG = 21,
    JRLE = 22,
    ZRLE2 = 24,
    DesktopSizePseudo = -223,
    LastRectPseudo = -224,
    CursorPseudo = -239,
    ContinuousUpdatesPseudo = -313,
}

pub trait Encoding: Send {
    fn get_type(&self) -> EncodingType;

    /// Transform this encoding from its representation into a byte vector that
    /// can be passed to the client.
    fn encode(&self) -> &[u8];
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

    fn encode(&self) -> &[u8] {
        &self.pixels
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
