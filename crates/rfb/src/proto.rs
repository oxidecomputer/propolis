// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2022 Oxide Computer Company

use std::mem::size_of;

use bitflags::bitflags;
use rgb_frame::FourCC;
use strum::FromRepr;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::bytes::{Buf, BytesMut};
use tokio_util::codec::Decoder;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::encodings::{Encoding, EncodingType};
use crate::keysym::KeySym;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("invalid protocol version")]
    InvalidProtocolVersion,

    #[error("invalid security type: {0}")]
    InvalidSecurityType(u8),

    #[error("invalid text encoding")]
    InvalidTextEncoding,

    #[error("unknown client message type ({0})")]
    UnknownMessageType(u8),

    #[error("unknown keysym: {0}")]
    UnknownKeysym(u32),

    #[error("message too large: {0}")]
    TooLarge(usize),

    #[error("unsupported feature: {0}")]
    UnsupportedFeat(&'static str),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

#[derive(Debug, Copy, Clone, PartialEq, PartialOrd)]
pub enum ProtoVersion {
    Rfb33,
    Rfb37,
    Rfb38,
}

impl ProtoVersion {
    pub async fn read_from(
        stream: &mut (impl AsyncRead + Unpin),
    ) -> Result<Self> {
        let mut buf = [0u8; 12];
        stream.read_exact(&mut buf).await?;

        match &buf {
            b"RFB 003.003\n" => Ok(ProtoVersion::Rfb33),
            b"RFB 003.007\n" => Ok(ProtoVersion::Rfb37),
            b"RFB 003.008\n" => Ok(ProtoVersion::Rfb38),
            _ => Err(ProtocolError::InvalidProtocolVersion),
        }
    }
    pub async fn write_to(
        self,
        stream: &mut (impl AsyncWrite + Unpin),
    ) -> Result<()> {
        let s = match self {
            ProtoVersion::Rfb33 => b"RFB 003.003\n",
            ProtoVersion::Rfb37 => b"RFB 003.007\n",
            ProtoVersion::Rfb38 => b"RFB 003.008\n",
        };

        Ok(stream.write_all(s).await?)
    }
}

// Section 7.1.2
#[derive(Debug, Clone)]
pub struct SecurityTypes(pub Vec<SecurityType>);

#[derive(Clone, PartialEq, Debug)]
pub enum SecurityType {
    None,
    VncAuthentication,
}

impl SecurityTypes {
    pub async fn write_to(
        self,
        stream: &mut (impl AsyncWrite + Unpin),
    ) -> Result<()> {
        // TODO: fix cast
        stream.write_u8(self.0.len() as u8).await?;
        for t in self.0.into_iter() {
            t.write_to(stream).await?;
        }

        Ok(())
    }
}

impl SecurityType {
    pub async fn read_from(
        stream: &mut (impl AsyncRead + Unpin),
    ) -> Result<Self> {
        let t = stream.read_u8().await?;
        match t {
            1 => Ok(SecurityType::None),
            2 => Ok(SecurityType::VncAuthentication),
            v => Err(ProtocolError::InvalidSecurityType(v)),
        }
    }
    pub async fn write_to(
        self,
        stream: &mut (impl AsyncWrite + Unpin),
    ) -> Result<()> {
        let val = match self {
            SecurityType::None => 0,
            SecurityType::VncAuthentication => 1,
        };
        stream.write_u8(val).await?;

        Ok(())
    }
}

// Section 7.1.3
pub enum SecurityResult {
    Success,
    Failure(String),
}

impl SecurityResult {
    pub async fn write_to(
        self,
        stream: &mut (impl AsyncWrite + Unpin),
    ) -> Result<()> {
        match self {
            SecurityResult::Success => {
                stream.write_u32(0).await?;
            }
            SecurityResult::Failure(s) => {
                stream.write_u32(1).await?;
                stream.write_all(s.as_bytes()).await?;
            }
        };
        Ok(())
    }
}

// Section 7.3.1
#[derive(Debug)]
pub struct ClientInit {
    pub shared: bool,
}

impl ClientInit {
    pub async fn read_from(
        stream: &mut (impl AsyncRead + Unpin),
    ) -> Result<Self> {
        let flag = stream.read_u8().await?;
        Ok(ClientInit { shared: flag != 0 })
    }
}

// Section 7.3.2
#[derive(Debug)]
pub struct ServerInit {
    pub initial_resolution: Resolution,
    pub pixel_format: PixelFormat,
    pub name: String,
}

impl ServerInit {
    pub async fn write_to(
        self,
        stream: &mut (impl AsyncWrite + Unpin),
    ) -> Result<()> {
        self.initial_resolution.write_to(stream).await?;
        self.pixel_format.write_to(stream).await?;

        // TODO: cast properly
        stream.write_u32(self.name.len() as u32).await?;
        stream.write_all(self.name.as_bytes()).await?;

        Ok(())
    }
}

pub struct FramebufferUpdate(pub Vec<Rectangle>);

impl FramebufferUpdate {
    pub async fn write_to(
        self,
        stream: &mut (impl AsyncWrite + Unpin),
    ) -> Result<()> {
        let header = raw::FramebufferUpdateHeader::new(self.0.len() as u16);
        stream.write_all(header.as_bytes()).await?;

        // rectangles
        for r in self.0.into_iter() {
            r.write_to(stream).await?;
        }

        Ok(())
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Position {
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Copy, Clone)]
pub struct Resolution {
    pub width: u16,
    pub height: u16,
}

impl Resolution {
    pub async fn write_to(
        self,
        stream: &mut (impl AsyncWrite + Unpin),
    ) -> Result<()> {
        stream.write_u16(self.width).await?;
        stream.write_u16(self.height).await?;
        Ok(())
    }
}

pub struct Rectangle {
    pub position: Position,
    pub dimensions: Resolution,
    pub data: Box<dyn Encoding>,
}

impl Rectangle {
    pub async fn write_to(
        self,
        stream: &mut (impl AsyncWrite + Unpin),
    ) -> Result<()> {
        stream.write_u16(self.position.x).await?;
        stream.write_u16(self.position.y).await?;
        stream.write_u16(self.dimensions.width).await?;
        stream.write_u16(self.dimensions.height).await?;
        stream.write_i32(self.data.get_type() as i32).await?;

        let data = self.data.encode();
        stream.write_all(data).await?;

        Ok(())
    }
}

// Section 7.4
#[derive(Debug, Clone, PartialEq, Immutable)]
pub struct PixelFormat {
    pub bits_per_pixel: u8, // TODO: must be 8, 16, or 32
    pub depth: u8,          // TODO: must be < bits_per_pixel
    pub big_endian: bool,
    pub color_spec: ColorSpecification,
}

impl PixelFormat {
    pub async fn write_to(
        self,
        stream: &mut (impl AsyncWrite + Unpin),
    ) -> Result<()> {
        let raw: raw::PixelFormat = self.into();
        stream.write_all(raw.as_bytes()).await?;
        Ok(())
    }
}
impl TryFrom<raw::PixelFormat> for PixelFormat {
    type Error = ProtocolError;

    fn try_from(
        value: raw::PixelFormat,
    ) -> std::result::Result<Self, Self::Error> {
        if value.true_color_flag == 0 {
            // Punt until we choose to support ColorMap
            Err(ProtocolError::UnsupportedFeat("ColorMap (non-truecolor) spec"))
        } else {
            let color_spec = ColorSpecification::ColorFormat(ColorFormat {
                red_max: value.red_max.get(),
                green_max: value.green_max.get(),
                blue_max: value.blue_max.get(),
                red_shift: value.red_shift,
                green_shift: value.green_shift,
                blue_shift: value.blue_shift,
            });

            Ok(Self {
                bits_per_pixel: value.bits_per_pixel,
                depth: value.depth,
                big_endian: value.big_endian_flag != 0,
                color_spec,
            })
        }
    }
}
impl From<PixelFormat> for raw::PixelFormat {
    fn from(value: PixelFormat) -> Self {
        let PixelFormat {
            bits_per_pixel,
            depth,
            big_endian,
            color_spec:
                ColorSpecification::ColorFormat(ColorFormat {
                    red_max,
                    green_max,
                    blue_max,
                    red_shift,
                    green_shift,
                    blue_shift,
                }),
        } = value;

        Self {
            bits_per_pixel,
            depth,
            big_endian_flag: big_endian as u8,
            // Without ColorMap support, all PFs are true-color for now
            true_color_flag: 1,
            red_max: red_max.into(),
            green_max: green_max.into(),
            blue_max: blue_max.into(),
            red_shift,
            green_shift,
            blue_shift,
            _padding: [0; 3],
        }
    }
}

// While rgb_frame supports only 4-byte truecolor formats, we can make some
// simple assumptions about PixelFormat conversions.
impl From<FourCC> for PixelFormat {
    fn from(value: FourCC) -> Self {
        let idx = value.le_idx_rgba();
        PixelFormat {
            bits_per_pixel: 32,
            depth: 24,
            big_endian: false,
            color_spec: ColorSpecification::ColorFormat(ColorFormat {
                red_max: 255,
                green_max: 255,
                blue_max: 255,
                red_shift: idx.0 as u8 * 8,
                green_shift: idx.1 as u8 * 8,
                blue_shift: idx.2 as u8 * 8,
            }),
        }
    }
}
impl TryInto<FourCC> for &PixelFormat {
    type Error = &'static str;

    fn try_into(self) -> std::result::Result<FourCC, Self::Error> {
        if self.bits_per_pixel != 32 || self.depth != 24 {
            return Err("format is not 4-bytes-per-pixel truecolor");
        }
        if self.big_endian {
            return Err("big endian not supported");
        }
        let PixelFormat { color_spec, .. } = self;
        let ColorSpecification::ColorFormat(cformat) = color_spec;
        if cformat.red_max != 255
            || cformat.green_max != 255
            || cformat.blue_max != 255
        {
            return Err("max color values for truecolor not found");
        }
        match (cformat.red_shift, cformat.green_shift, cformat.blue_shift) {
            (0, 8, 16) => Ok(FourCC::XB24),
            (16, 8, 0) => Ok(FourCC::XR24),
            _ => Err("matching color shifts not found"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Immutable)]
pub enum ColorSpecification {
    ColorFormat(ColorFormat),
    // Not covered: colormap support
}

#[derive(Debug, Clone, PartialEq, Immutable)]
pub struct ColorFormat {
    // TODO: maxes must be 2^N - 1 for N bits per color
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

// Section 7.5
#[derive(Debug)]
pub enum ClientMessage {
    SetPixelFormat(PixelFormat),
    SetEncodings {
        /// Encodings with a type recognized by this crate
        encodings: Vec<EncodingType>,
        /// Raw values of unrecognized encodings
        unknown: Vec<i32>,
    },
    FramebufferUpdateRequest(FramebufferUpdateRequest),
    KeyEvent(KeyEvent),
    PointerEvent(PointerEvent),
    ClientCutText(String),
}

#[derive(FromRepr)]
#[repr(u8)]
enum ClientMessageType {
    SetPixelFormat = 0,
    SetEncodings = 2,
    FramebufferUpdateRequest = 3,
    KeyEvent = 4,
    PointerEvent = 5,
    ClientCutText = 6,
}

fn read_data<T: FromBytes>(buf: &mut BytesMut) -> Option<T> {
    let sz = size_of::<T>();
    // It'd be kind of nice to return the error here instead of an Option, but
    // because the error borrows the buf we're going to try parsing from, rustc
    // believes the buffer to be immutably borrowed when we advance it below.
    // As an option the Err and its borrow are discarded so we avoid the issue.
    let data = T::read_from_prefix(buf).ok()?.0;
    buf.advance(sz);
    Some(data)
}

pub struct ClientMessageDecoder {
    /// Limit to how many bytes decode is willing to buffer for client messages
    pub buffer_limit: usize,
}
impl Default for ClientMessageDecoder {
    fn default() -> Self {
        // Client messages are small, so 16k should be more than enough
        Self { buffer_limit: 0x10000 }
    }
}

impl Decoder for ClientMessageDecoder {
    type Item = ClientMessage;
    type Error = ProtocolError;

    fn decode(
        &mut self,
        src: &mut BytesMut,
    ) -> std::result::Result<Option<Self::Item>, Self::Error> {
        if src.is_empty() {
            return Ok(None);
        }
        let type_byte = src[0];
        let message_type = ClientMessageType::from_repr(type_byte)
            .ok_or(ProtocolError::UnknownMessageType(type_byte))?;

        let msg_sz_reqd = match message_type {
            ClientMessageType::SetPixelFormat => {
                // 3 bytes padding + message
                3 + size_of::<raw::PixelFormat>()
            }

            ClientMessageType::SetEncodings => {
                if src.len() < 4 {
                    return Ok(None);
                }

                // 1 byte padding + u16 len + len * u32
                let num_encoding =
                    u16::from_be_bytes(src[2..4].try_into().unwrap());
                1 + size_of::<u16>()
                    + (num_encoding as usize * size_of::<u32>())
            }

            ClientMessageType::FramebufferUpdateRequest => {
                size_of::<raw::FramebufferUpdateRequest>()
            }
            ClientMessageType::KeyEvent => size_of::<raw::KeyEvent>(),
            ClientMessageType::PointerEvent => size_of::<raw::PointerEvent>(),
            ClientMessageType::ClientCutText => {
                if src.len() < 8 {
                    return Ok(None);
                }
                // 3 bytes of padding + i32 len + string
                let data_len =
                    u32::from_be_bytes(src[4..8].try_into().unwrap());
                1 + size_of::<i32>() + data_len as usize
            }
        };
        let total_sz_reqd = 1 + msg_sz_reqd;
        if total_sz_reqd > self.buffer_limit {
            return Err(ProtocolError::TooLarge(msg_sz_reqd));
        }
        if src.len() < total_sz_reqd {
            if src.capacity() < total_sz_reqd {
                src.reserve(total_sz_reqd - src.capacity());
            }
            return Ok(None);
        }

        // Now that we're sure that enough data is buffered to decode a whole
        // message, consume the type byte, and pass the rest on to the decoding
        // logic.
        src.advance(1);
        match message_type {
            ClientMessageType::SetPixelFormat => {
                // 3 bytes padding
                src.advance(3);
                let raw = read_data::<raw::PixelFormat>(src).unwrap();
                Ok(Some(ClientMessage::SetPixelFormat(raw.try_into()?)))
            }
            ClientMessageType::SetEncodings => {
                // 1 byte padding
                src.advance(1);

                let count = src.get_u16() as usize;
                let mut encodings = Vec::with_capacity(count);
                let mut unknown = Vec::new();
                for _n in 0..count {
                    let raw = src.get_i32();
                    match EncodingType::from_repr(raw) {
                        Some(enc) => encodings.push(enc),
                        None => unknown.push(raw),
                    }
                }
                Ok(Some(ClientMessage::SetEncodings { encodings, unknown }))
            }
            ClientMessageType::FramebufferUpdateRequest => {
                let raw =
                    read_data::<raw::FramebufferUpdateRequest>(src).unwrap();
                Ok(Some(ClientMessage::FramebufferUpdateRequest(raw.into())))
            }
            ClientMessageType::KeyEvent => {
                let raw = read_data::<raw::KeyEvent>(src).unwrap();
                let converted: KeyEvent = raw.try_into()?;
                Ok(Some(ClientMessage::KeyEvent(converted)))
            }
            ClientMessageType::PointerEvent => {
                let raw = read_data::<raw::PointerEvent>(src).unwrap();
                Ok(Some(ClientMessage::PointerEvent(raw.into())))
            }
            ClientMessageType::ClientCutText => {
                // 3 bytes padding
                src.advance(3);

                let len = src.get_u32() as usize;
                let buf = src[..len].to_vec();

                // TODO: The encoding RFB uses is ISO 8859-1 (Latin-1), which is
                // a subset of utf-8. Determine if this is the right approach.
                let text = String::from_utf8(buf)
                    .map_err(|_| ProtocolError::InvalidTextEncoding)?;

                Ok(Some(ClientMessage::ClientCutText(text)))
            }
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct FramebufferUpdateRequest {
    pub incremental: bool,
    pub position: Position,
    pub resolution: Resolution,
}

impl From<raw::FramebufferUpdateRequest> for FramebufferUpdateRequest {
    fn from(value: raw::FramebufferUpdateRequest) -> Self {
        Self {
            incremental: value.incremental != 0,
            position: value.position.into(),
            resolution: value.resolution.into(),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct KeyEvent {
    pub is_pressed: bool,
    pub keysym: KeySym,
    pub keysym_raw: u32,
}
impl TryFrom<raw::KeyEvent> for KeyEvent {
    type Error = ProtocolError;

    fn try_from(
        value: raw::KeyEvent,
    ) -> std::result::Result<Self, Self::Error> {
        let keysym_raw = value.key.get();
        let keysym = KeySym::try_from(keysym_raw)
            .or(Err(ProtocolError::UnknownKeysym(keysym_raw)))?;
        Ok(Self { is_pressed: value.down_flag != 0, keysym, keysym_raw })
    }
}

bitflags! {
    #[derive(Debug, Copy, Clone)]
    pub struct MouseButtons: u8 {
        const LEFT = 1 << 0;
        const MIDDLE = 1 << 1;
        const RIGHT = 1 << 2;
        const SCROLL_A = 1 << 3;
        const SCROLL_B = 1 << 4;
        const SCROLL_C = 1 << 5;
        const SCROLL_D = 1 << 6;
    }
}

#[derive(Debug, Copy, Clone)]
pub struct PointerEvent {
    pub position: Position,
    pub pressed: MouseButtons,
}
impl From<raw::PointerEvent> for PointerEvent {
    fn from(value: raw::PointerEvent) -> Self {
        Self {
            position: value.position.into(),
            pressed: MouseButtons::from_bits_truncate(value.button_mask),
        }
    }
}

mod raw {
    use zerocopy::big_endian::{U16, U32};
    use zerocopy::{FromBytes, Immutable, IntoBytes};

    #[allow(dead_code)]
    #[derive(Copy, Clone, FromBytes, IntoBytes, Immutable)]
    #[repr(C, packed)]
    pub(crate) struct PixelFormat {
        pub bits_per_pixel: u8,
        pub depth: u8,
        pub big_endian_flag: u8,
        pub true_color_flag: u8,
        pub red_max: U16,
        pub green_max: U16,
        pub blue_max: U16,
        pub red_shift: u8,
        pub green_shift: u8,
        pub blue_shift: u8,
        pub _padding: [u8; 3],
    }

    #[derive(Copy, Clone, FromBytes, Immutable)]
    #[repr(C, packed)]
    pub(crate) struct FramebufferUpdateRequest {
        pub incremental: u8,
        pub position: Position,
        pub resolution: Resolution,
    }

    #[derive(Copy, Clone, FromBytes)]
    #[repr(C, packed)]
    pub(crate) struct KeyEvent {
        pub down_flag: u8,
        pub _padding: [u8; 2],
        pub key: U32,
    }

    #[derive(Copy, Clone, FromBytes)]
    #[repr(C, packed)]
    pub(crate) struct PointerEvent {
        pub button_mask: u8,
        pub position: Position,
    }

    #[derive(Copy, Clone, FromBytes, Immutable)]
    #[repr(C, packed)]
    pub(crate) struct Position {
        pub x: U16,
        pub y: U16,
    }
    impl From<Position> for super::Position {
        fn from(value: Position) -> Self {
            Self { x: value.x.get(), y: value.y.get() }
        }
    }

    #[derive(Copy, Clone, FromBytes, Immutable)]
    #[repr(C, packed)]
    pub(crate) struct Resolution {
        width: U16,
        height: U16,
    }
    impl From<Resolution> for super::Resolution {
        fn from(value: Resolution) -> Self {
            Self { width: value.width.get(), height: value.height.get() }
        }
    }

    #[derive(Copy, Clone, IntoBytes, Immutable)]
    #[repr(C, packed)]
    #[allow(dead_code)]
    pub(crate) struct FramebufferUpdateHeader {
        msg_type: u8,
        _padding: u8,
        pub num_rects: U16,
    }
    impl FramebufferUpdateHeader {
        pub fn new(num_rects: u16) -> Self {
            Self { msg_type: 0, _padding: 0, num_rects: U16::new(num_rects) }
        }
    }
}
