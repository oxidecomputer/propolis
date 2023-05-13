//! Copyright 2021 Oxide Computer Company
//!
//! Support for encoding messages in the propolis/bhyve live
//! migration protocol. Messages are serialized to binary and
//! wrapped in Binary websocket frames with a trailing byte
//! indicating the message type.
//!
//! As defined in RFD0071, most messages are either serialized
//! structures or blobs, while the structures involved in the
//! memory transfer phases of the protocols are directly serialized
//! binary structures.  We represent each of these structures in a
//! dedicated message type; similarly with 4KiB "page" data, etc.
//! Serialized structures are assumed to be text.
//!
//! Several messages involved in memory transfer include bitmaps
//! that are nominally bounded by associated [start, end) address
//! ranges.  However, the framing layer makes no effort to validate
//! the implied invariants: higher level software is responsible
//! for that.

use super::MigrateError;
use bytes::{Buf, BufMut, Bytes};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use slog::error;
use std::convert::TryFrom;
use thiserror::Error;
use tokio_tungstenite::tungstenite;

/// Migration protocol errors.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// We received an unexpected message type
    #[error("couldn't decode message type ({0})")]
    InvalidMessageType(u8),

    /// The message received on the wire wasn't the expected length
    #[error("unexpected message length {1} for type {0:?}")]
    UnexpectedMessageLen(u8, usize),

    /// Encountered an I/O error on the transport
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to serialize or deserialize a message
    #[error("serialization error: {0}")]
    Ron(#[from] ron::Error),

    /// Received non-UTF8 string
    #[error("non-UTF8 string: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    /// Nothing, not even a tag byte
    #[error("received empty message with no discriminant")]
    EmptyMessage,

    /// An error occurred in the underlying websocket transport
    #[error("error occurred in websocket layer: {0}")]
    WebsocketError(tokio_tungstenite::tungstenite::Error),

    /// All our codec's messages should be tungstenite::Message::Binary
    #[error("received empty message with no discriminant")]
    UnexpectedWebsocketMessage(tungstenite::Message),
}

/// Message represents the different frame types for messages
/// exchanged in the live migration protocol.  Most structured
/// data is serialized into a string, while blobs are uninterpreted
/// vectors of bytes and 4KiB pages (e.g. of RAM) are uninterpreted
/// fixed-sized arrays.  The memory-related messages are nominally
/// structured, but given the overall volume of memory data exchanged,
/// we serialize and deserialize them directly.
#[derive(Debug)]
pub(crate) enum Message {
    Okay,
    Error(MigrateError),
    Serialized(String),
    Blob(Vec<u8>),
    Page(Vec<u8>),
    MemQuery(u64, u64),
    MemOffer(u64, u64, Vec<u8>),
    MemEnd(u64, u64),
    MemFetch(u64, u64, Vec<u8>),
    MemXfer(u64, u64, Vec<u8>),
    MemDone,
}

/// MessageType represents tags that are used in the protocol for
/// identifying frame types.  They are an implementation detail of
/// the wire format, and not used elsewhere.  However, they must be
/// kept in bijection with Message, above.
#[derive(Debug, PartialEq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
enum MessageType {
    Okay,
    Error,
    Serialized,
    Blob,
    Page,
    MemQuery,
    MemOffer,
    MemEnd,
    MemFetch,
    MemXfer,
    MemDone,
}

/// By implementing `From<&Message>` on MessageType, we can translate
/// each message into its tag type, ensuring full coverage.
impl From<&Message> for MessageType {
    fn from(m: &Message) -> MessageType {
        match m {
            Message::Okay => MessageType::Okay,
            Message::Error(_) => MessageType::Error,
            Message::Serialized(_) => MessageType::Serialized,
            Message::Blob(_) => MessageType::Blob,
            Message::Page(_) => MessageType::Page,
            Message::MemQuery(_, _) => MessageType::MemQuery,
            Message::MemOffer(_, _, _) => MessageType::MemOffer,
            Message::MemEnd(_, _) => MessageType::MemEnd,
            Message::MemFetch(_, _, _) => MessageType::MemFetch,
            Message::MemXfer(_, _, _) => MessageType::MemXfer,
            Message::MemDone => MessageType::MemDone,
        }
    }
}

impl std::convert::TryInto<tungstenite::Message> for Message {
    type Error = ProtocolError;
    fn try_into(self) -> Result<tungstenite::Message, ProtocolError> {
        let mut dst = Vec::new();
        let tag = MessageType::from(&self) as u8;
        match self {
            Message::Okay | Message::MemDone => {}
            Message::Error(e) => {
                let serialized = ron::ser::to_string(&e)?;
                dst.extend(serialized.as_bytes());
            }
            Message::Serialized(s) => dst.put_slice(s.as_bytes()),
            Message::Blob(bytes) | Message::Page(bytes) => {
                dst.put_slice(&bytes);
            }
            Message::MemQuery(start, end) | Message::MemEnd(start, end) => {
                dst.put_u64_le(start);
                dst.put_u64_le(end);
            }
            Message::MemOffer(start, end, bitmap)
            | Message::MemFetch(start, end, bitmap)
            | Message::MemXfer(start, end, bitmap) => {
                dst.put_u64_le(start);
                dst.put_u64_le(end);
                dst.put_slice(&bitmap);
            }
        }
        // tag at the end so we can pop it later (& so u64's align nicely)
        dst.push(tag);
        Ok(tungstenite::Message::Binary(dst))
    }
}

// Retrieves a (`start`, `end`) pair from the buffer, ensuring valid length.
fn get_start_end(
    tag: MessageType,
    src: &mut Bytes,
) -> Result<(u64, u64), ProtocolError> {
    if src.len() < 16 {
        return Err(ProtocolError::UnexpectedMessageLen(tag as u8, src.len()));
    }
    let start = src.get_u64_le();
    let end = src.get_u64_le();
    Ok((start, end))
}

impl std::convert::TryInto<Message> for tungstenite::Message {
    type Error = ProtocolError;
    fn try_into(self) -> Result<Message, ProtocolError> {
        match self {
            tungstenite::Message::Binary(mut v) => {
                // If the tag byte is absent or invalid, don't bother looking at the message.
                let tag_byte = v.pop().ok_or(ProtocolError::EmptyMessage)?;
                let tag = MessageType::try_from(tag_byte)
                    .map_err(|_| ProtocolError::InvalidMessageType(tag_byte))?;
                let mut src = Bytes::from(v);
                // At this point, we have a valid message of a known type, and
                // the remaining bytes are the message contents.
                // Attempt decode and return the received message.
                let m = match tag {
                    MessageType::Okay => {
                        if !src.is_empty() {
                            return Err(ProtocolError::UnexpectedMessageLen(
                                tag as u8,
                                src.len(),
                            ));
                        }
                        Message::Okay
                    }
                    MessageType::Error => {
                        let e = ron::de::from_str(std::str::from_utf8(&src)?)?;
                        Message::Error(e)
                    }
                    MessageType::Serialized => {
                        let s = std::str::from_utf8(&src)?.to_string();
                        Message::Serialized(s)
                    }
                    MessageType::Blob => Message::Blob(src.to_vec()),
                    MessageType::Page => {
                        if src.len() != 4096 {
                            return Err(ProtocolError::UnexpectedMessageLen(
                                tag as u8,
                                src.len(),
                            ));
                        }
                        Message::Page(src.to_vec())
                    }
                    MessageType::MemQuery => {
                        let (start, end) = get_start_end(tag, &mut src)?;
                        Message::MemQuery(start, end)
                    }
                    MessageType::MemOffer => {
                        let (start, end) = get_start_end(tag, &mut src)?;
                        let bitmap = src.to_vec();
                        Message::MemOffer(start, end, bitmap)
                    }
                    MessageType::MemEnd => {
                        let (start, end) = get_start_end(tag, &mut src)?;
                        Message::MemEnd(start, end)
                    }
                    MessageType::MemFetch => {
                        let (start, end) = get_start_end(tag, &mut src)?;
                        let bitmap = src.to_vec();
                        Message::MemFetch(start, end, bitmap)
                    }
                    MessageType::MemXfer => {
                        let (start, end) = get_start_end(tag, &mut src)?;
                        let bitmap = src.to_vec();
                        Message::MemXfer(start, end, bitmap)
                    }
                    MessageType::MemDone => {
                        if !src.is_empty() {
                            return Err(ProtocolError::UnexpectedMessageLen(
                                tag as u8,
                                src.len(),
                            ));
                        }
                        Message::MemDone
                    }
                };
                Ok(m)
            }
            x => Err(ProtocolError::UnexpectedWebsocketMessage(x)),
        }
    }
}

#[cfg(test)]
mod encoder_tests {
    use super::*;
    use std::convert::TryInto;
    use tokio_tungstenite::tungstenite;

    fn encode(m: Message) -> Vec<u8> {
        if let tungstenite::Message::Binary(bytes) = m.try_into().unwrap() {
            bytes
        } else {
            panic!();
        }
    }

    #[test]
    fn encode_okay() {
        let bytes = encode(Message::Okay);
        assert_eq!(&bytes[..], &[MessageType::Okay as u8]);
    }

    #[test]
    fn encode_error() {
        let error = MigrateError::Initiate;
        let mut bytes = encode(Message::Error(error));
        assert_eq!(bytes.pop(), Some(MessageType::Error as u8));
        assert_eq!(&bytes[..], br#"Initiate"#);
    }

    #[test]
    fn encode_serialized() {
        let obj = String::from("this is an object");
        let mut bytes = encode(Message::Serialized(obj));
        assert_eq!(bytes.pop(), Some(MessageType::Serialized as u8));
        assert_eq!(&bytes[..], b"this is an object");
    }

    #[test]
    fn encode_empty_blob() {
        let empty = Vec::new();
        let bytes = encode(Message::Blob(empty));
        assert_eq!(&bytes[..], &[MessageType::Blob as u8]);
    }

    #[test]
    fn encode_blob() {
        let nonempty = vec![1, 2, 3, 4];
        let bytes = encode(Message::Blob(nonempty));
        assert_eq!(&bytes[..], &[1, 2, 3, 4, MessageType::Blob as u8]);
    }

    #[test]
    fn encode_page() {
        let page = [0u8; 4096];
        let mut bytes = encode(Message::Page(page.to_vec()));
        assert_eq!(bytes.pop(), Some(MessageType::Page as u8));
        assert_eq!(bytes, page);
    }

    #[test]
    fn encode_mem_query() {
        let mut bytes = encode(Message::MemQuery(1, 2));
        assert_eq!(bytes.pop(), Some(MessageType::MemQuery as u8));
        assert_eq!(&bytes[..8], &[1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[8..], &[2, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn encode_mem_offer() {
        let mut bytes = encode(Message::MemOffer(0, 0x8000, vec![0b1010_0101]));
        assert_eq!(bytes.pop(), Some(MessageType::MemOffer as u8));
        assert_eq!(&bytes[..8], &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[8..8 + 8], &[0, 0b1000_0000, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[8 + 8..], &[0b1010_0101]);
    }

    #[test]
    fn encode_mem_end() {
        let mut bytes = encode(Message::MemEnd(0, 8 * 4096));
        assert_eq!(bytes.pop(), Some(MessageType::MemEnd as u8));
        assert_eq!(&bytes[..8], &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[8..], &[0, 0b1000_0000, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn encode_mem_fetch() {
        let mut bytes = encode(Message::MemFetch(0, 0x4000, vec![0b0000_0101]));
        assert_eq!(bytes.pop(), Some(MessageType::MemFetch as u8));
        assert_eq!(&bytes[..8], &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[8..8 + 8], &[0, 0x40, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[8 + 8..], &[0b0000_0101]);
    }

    #[test]
    fn encode_mem_xfer() {
        let mut bytes = encode(Message::MemXfer(0, 0x8000, vec![0b1010_0101]));
        assert_eq!(bytes.pop(), Some(MessageType::MemXfer as u8));
        assert_eq!(&bytes[..8], &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[8..8 + 8], &[0, 0x80, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[8 + 8..], &[0b1010_0101]);
    }

    #[test]
    fn encode_mem_done() {
        let bytes = encode(Message::MemDone);
        assert_eq!(&bytes[..], [MessageType::MemDone as u8]);
    }
}

#[cfg(test)]
mod live_migration_decoder_tests {
    use super::*;

    #[test]
    fn get_start_end_ok() {
        let one_two = &[1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0];
        let mut bytes = bytes::Bytes::from_static(one_two);
        let (start, end) =
            super::get_start_end(MessageType::MemFetch, &mut bytes).unwrap();
        assert_eq!(start, 1);
        assert_eq!(end, 2);
    }

    #[test]
    fn get_start_end_err() {
        let one_tw = &[1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0];
        let mut bytes = bytes::Bytes::from_static(one_tw);
        assert!(
            super::get_start_end(MessageType::MemFetch, &mut bytes).is_err()
        );
    }
}

#[cfg(test)]
mod decoder_tests {
    use super::*;
    use std::convert::TryInto;
    use tokio_tungstenite::tungstenite;

    #[test]
    fn decode_bad_tag_fails() {
        let bytes = vec![222];
        let res: Result<Message, _> =
            tungstenite::Message::Binary(bytes).try_into();
        assert!(res.is_err());
    }

    #[test]
    fn decode_nonbinary_fails() {
        let res: Result<Message, _> =
            tungstenite::Message::Text(String::new()).try_into();
        assert!(res.is_err());
    }

    #[test]
    fn decode_tagless_fails() {
        let res: Result<Message, _> =
            tungstenite::Message::Binary(vec![]).try_into();
        assert!(res.is_err());
    }

    #[test]
    fn decode_error() {
        let mut bytes = br#"Websocket("foo")"#.to_vec();
        bytes.push(MessageType::Error as u8);
        let expected = MigrateError::Websocket("foo".into());
        let decoded = tungstenite::Message::Binary(bytes).try_into().unwrap();
        assert!(matches!(decoded, Message::Error(e) if e == expected));
    }

    #[test]
    fn decode_blob() {
        let mut bytes = b"asdf".to_vec();
        bytes.push(MessageType::Blob as u8);
        let decoded = tungstenite::Message::Binary(bytes).try_into().unwrap();
        assert!(matches!(decoded, Message::Blob(b) if b == b"asdf".to_vec()));
    }

    #[test]
    fn decode_page() {
        let mut bytes = vec![0u8; 4096];
        bytes.push(MessageType::Page as u8);
        let decoded = tungstenite::Message::Binary(bytes).try_into().unwrap();
        assert!(matches!(decoded, Message::Page(p)
            if p.iter().all(|&b| b == 0)));
    }

    #[test]
    fn decode_mem_query() {
        let mut bytes = vec![1, 0, 0, 0, 0, 0, 0, 0];
        bytes.extend(&[2, 0, 0, 0, 0, 0, 0, 0]);
        bytes.push(MessageType::MemQuery as u8);
        let decoded = tungstenite::Message::Binary(bytes).try_into().unwrap();
        assert!(matches!(decoded, Message::MemQuery(start, end)
            if start == 1 && end == 2));
    }

    #[test]
    fn decode_mem_offer() {
        let mut bytes = vec![0, 0, 0, 0, 0, 0, 0, 0];
        bytes.extend(&[0, 0x80, 0, 0, 0, 0, 0, 0]);
        bytes.push(0b0000_1111);
        bytes.push(MessageType::MemOffer as u8);
        let decoded = tungstenite::Message::Binary(bytes).try_into().unwrap();
        assert!(matches!(decoded, Message::MemOffer(start, end, v)
            if start == 0 && end == 0x8000 && v == vec![0b0000_1111]));
    }

    #[test]
    fn decode_mem_offer_long_bitmap() {
        let mut bytes = vec![0, 0, 0, 0, 0, 0, 0, 0];
        bytes.extend(&[0, 0x80, 0, 0, 0, 0, 0, 0]);
        bytes.push(0b0000_1111);
        bytes.push(0b0000_1010);
        bytes.push(MessageType::MemOffer as u8);
        let decoded = tungstenite::Message::Binary(bytes).try_into().unwrap();
        assert!(matches!(decoded, Message::MemOffer(start, end, v)
            if start == 0 &&
                end == 0x8000 &&
                v == vec![0b0000_1111, 0b0000_1010]));
    }

    #[test]
    fn decode_mem_end() {
        let mut bytes = vec![0, 0x40, 0, 0, 0, 0, 0, 0];
        bytes.extend(&[0, 0x40 + 0x80, 0, 0, 0, 0, 0, 0]);
        bytes.push(MessageType::MemEnd as u8);
        let decoded = tungstenite::Message::Binary(bytes).try_into().unwrap();
        assert!(matches!(decoded, Message::MemEnd(start, end)
            if start == 0x4000 && end == 0xC000));
    }

    #[test]
    fn decode_mem_fetch() {
        let mut bytes = vec![0, 0, 0, 0, 0, 0, 0, 0];
        bytes.extend(&[0, 0x80, 0, 0, 0, 0, 0, 0]);
        bytes.push(0b0000_1111);
        bytes.push(MessageType::MemFetch as u8);
        let decoded = tungstenite::Message::Binary(bytes).try_into().unwrap();
        assert!(matches!(decoded, Message::MemFetch(start, end, v)
            if start == 0 && end == 0x8000 && v == vec![0b0000_1111]));
    }

    #[test]
    fn decode_mem_xfer() {
        let mut bytes = vec![0, 0, 0, 0, 0, 0, 0, 0];
        bytes.extend(&[0, 0x80, 0, 0, 0, 0, 0, 0]);
        bytes.push(0b0000_1111);
        bytes.push(MessageType::MemXfer as u8);
        let decoded = tungstenite::Message::Binary(bytes).try_into().unwrap();
        assert!(matches!(decoded, Message::MemXfer(start, end, v)
            if start == 0 && end == 0x8000 && v == vec![0b0000_1111]));
    }

    #[test]
    fn decode_mem_done() {
        let bytes = vec![MessageType::MemDone as u8];
        let decoded = tungstenite::Message::Binary(bytes).try_into().unwrap();
        assert!(matches!(decoded, Message::MemDone));
    }
}
