//! Copyright 2021 Oxide Computer Company
//!
//! Support for framing messages in the propolis/bhyve live
//! migration protocol.  Frames are defined by a 5-byte header
//! consisting of a 32-bit length (unsigned little endian)
//! followed by a tag byte indicating the frame type, and then
//! the frame data.  The length field includes the header.
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
use bytes::{Buf, BufMut, BytesMut};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use std::convert::TryFrom;
use tokio_util::codec;

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
#[derive(PartialEq, IntoPrimitive, TryFromPrimitive)]
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

/// The LiveMigratonEncoder ZST represents the right to encode
/// and decode messages into and from frames.
#[derive(Default)]
pub(crate) struct LiveMigrationFramer {}

impl LiveMigrationFramer {
    /// Creates a new LiveMigrationFramer, which represents the
    /// right to encode and decode messages.
    pub fn new() -> LiveMigrationFramer {
        LiveMigrationFramer::default()
    }
    /// Writes the header at the start of the frame.  Also reserves enough space
    /// in the destination buffer for the complete message.
    fn put_header(&mut self, tag: MessageType, len: usize, dst: &mut BytesMut) {
        let len = len + 5;
        if dst.remaining_mut() < len {
            dst.reserve(len - dst.remaining_mut());
        }
        dst.put_u32_le(len as u32);
        dst.put_u8(tag.into());
    }

    // Writes a (`start`, `end`) pair into the buffer.
    fn put_start_end(&mut self, start: u64, end: u64, dst: &mut BytesMut) {
        dst.put_u64_le(start);
        dst.put_u64_le(end);
    }

    // Writes a vector of bytes representing a bitmap into the buffer.
    fn put_bitmap(&mut self, bitmap: &[u8], dst: &mut BytesMut) {
        dst.put(bitmap);
    }

    // Retrieves a (`start`, `end`) pair from the buffer.  Ensures
    // valid length, and returns the length minus the size of the
    // pair.
    fn get_start_end(
        &mut self,
        len: usize,
        src: &mut BytesMut,
    ) -> Result<(usize, u64, u64), anyhow::Error> {
        if len < 16 {
            anyhow::bail!("short message reading start end");
        }
        let start = src.get_u64_le();
        let end = src.get_u64_le();
        Ok((len - 16, start, end))
    }

    // Retrieves a bitmap from the buffer.  Validates that enough
    // bytes are in the buffer to
    fn get_bitmap(
        &mut self,
        len: usize,
        src: &mut BytesMut,
    ) -> Result<Vec<u8>, anyhow::Error> {
        if src.remaining() < len {
            anyhow::bail!("short message reading bitmap");
        }
        let v = src[..len].to_vec();
        src.advance(len);
        Ok(v.to_vec())
    }
}

impl codec::Encoder<Message> for LiveMigrationFramer {
    type Error = anyhow::Error;

    // Encodes each message according to its type.
    fn encode(
        &mut self,
        m: Message,
        dst: &mut BytesMut,
    ) -> Result<(), Self::Error> {
        let tag = (&m).into();
        match m {
            Message::Okay => {
                self.put_header(tag, 0, dst);
            }
            Message::Error(e) => {
                let serialized = ron::ser::to_string(&e)?;
                let bytes = serialized.into_bytes();
                self.put_header(tag, bytes.len(), dst);
                dst.put(&bytes[..]);
            }
            Message::Serialized(s) => {
                let bytes = s.into_bytes();
                self.put_header(tag, bytes.len(), dst);
                dst.put(&bytes[..]);
            }
            Message::Blob(bytes) => {
                self.put_header(tag, bytes.len(), dst);
                dst.put(&bytes[..]);
            }
            Message::Page(page) => {
                self.put_header(tag, page.len(), dst);
                dst.put(&page[..]);
            }
            Message::MemQuery(start, end) => {
                self.put_header(tag, 8 + 8, dst);
                self.put_start_end(start, end, dst);
            }
            Message::MemOffer(start, end, bitmap) => {
                self.put_header(tag, 8 + 8 + bitmap.len(), dst);
                self.put_start_end(start, end, dst);
                self.put_bitmap(&bitmap, dst);
            }
            Message::MemEnd(start, end) => {
                self.put_header(tag, 8 + 8, dst);
                self.put_start_end(start, end, dst);
            }
            Message::MemFetch(start, end, bitmap) => {
                self.put_header(tag, 8 + 8 + bitmap.len(), dst);
                self.put_start_end(start, end, dst);
                self.put_bitmap(&bitmap, dst);
            }
            Message::MemXfer(start, end, bitmap) => {
                self.put_header(tag, 8 + 8 + bitmap.len(), dst);
                self.put_start_end(start, end, dst);
                self.put_bitmap(&bitmap, dst);
            }
            Message::MemDone => {
                self.put_header(tag, 0, dst);
            }
        };
        Ok(())
    }
}

impl codec::Decoder for LiveMigrationFramer {
    type Item = Message;
    type Error = anyhow::Error;

    // Decodes each message according to the header length and type
    // indicated by the tag byte.
    fn decode(
        &mut self,
        src: &mut BytesMut,
    ) -> Result<Option<Self::Item>, Self::Error> {
        // Each message is prepended with a 5 octet header.  The
        // first four of those contain a 32-bit little-endian frame
        // size and the fifth is a tag field.  Check whether the
        // underlying transport has produced at least 5 bytes for
        // that header.
        if src.remaining() < 5 {
            return Ok(None);
        }
        // Extract the frame header.  If the tag byte is invalid,
        // don't bother looking at the frame size.
        let tag = MessageType::try_from(src[4])?;
        let len = u32::from_le_bytes([src[0], src[1], src[2], src[3]]) as usize;
        if len < 5 {
            anyhow::bail!("bad length");
        }
        // The frame header looks valid; ensure we have read the entire
        // message.
        if src.remaining() < len {
            src.reserve(len - src.remaining());
            return Ok(None);
        }
        // At this point, we have a valid message of the specified length.
        // Advance past the frame header, attempt decode and return the
        // received message.
        src.advance(5);
        let len = len - 5;
        if tag == MessageType::Okay {
            assert_eq!(len, 0);
            return Ok(Some(Message::Okay));
        }
        let m = match tag {
            MessageType::Okay => {
                // We already handled Okay above, but we
                // add a throw-away case for it here instead
                // of a wildcard for complete enum coverage.
                // Should we add a case later, the type system
                // will ensure we've covered it here.
                anyhow::bail!("impossible okay")
            }
            MessageType::Error => {
                let e = ron::de::from_str(std::str::from_utf8(&src[..len])?)?;
                src.advance(len);
                Message::Error(e)
            }
            MessageType::Serialized => {
                let s = std::str::from_utf8(&src[..len])?.to_string();
                src.advance(len);
                Message::Serialized(s)
            }
            MessageType::Blob => {
                let v = src[..len].to_vec();
                src.advance(len);
                Message::Blob(v)
            }
            MessageType::Page => {
                if len != 4096 {
                    anyhow::bail!("bad message size");
                }
                let p = src[..len].to_vec();
                src.advance(len);
                Message::Page(p)
            }
            MessageType::MemQuery => {
                let (_, start, end) = self.get_start_end(len, src)?;
                Message::MemQuery(start, end)
            }
            MessageType::MemOffer => {
                let (len, start, end) = self.get_start_end(len, src)?;
                let bitmap = self.get_bitmap(len, src)?;
                Message::MemOffer(start, end, bitmap)
            }
            MessageType::MemEnd => {
                let (_, start, end) = self.get_start_end(len, src)?;
                Message::MemEnd(start, end)
            }
            MessageType::MemFetch => {
                let (len, start, end) = self.get_start_end(len, src)?;
                let bitmap = self.get_bitmap(len, src)?;
                Message::MemFetch(start, end, bitmap)
            }
            MessageType::MemXfer => {
                let (len, start, end) = self.get_start_end(len, src)?;
                let bitmap = self.get_bitmap(len, src)?;
                Message::MemXfer(start, end, bitmap)
            }
            MessageType::MemDone => {
                assert_eq!(len, 0);
                Message::MemDone
            }
        };
        Ok(Some(m))
    }
}

#[cfg(test)]
mod live_migration_encoder_tests {
    use super::*;

    #[test]
    fn put_header() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        encoder.put_header(MessageType::Okay, 0, &mut bytes);
        assert_eq!(&bytes[..], &[5, 0, 0, 0, 0]);
    }

    #[test]
    fn put_header_nonzero_tag() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        encoder.put_header(MessageType::Error, 0, &mut bytes);
        assert_eq!(&bytes[..], &[5, 0, 0, 0, 1]);
    }

    #[test]
    fn put_start_end() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        encoder.put_start_end(1, 2, &mut bytes);
        assert_eq!(
            &bytes[..],
            &[1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn put_empty_bitmap() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        let v = Vec::new();
        encoder.put_bitmap(&v, &mut bytes);
        assert!(&bytes[..].is_empty());
    }

    #[test]
    fn put_bitmap() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        let v = vec![0b1100_0000];
        encoder.put_bitmap(&v, &mut bytes);
        assert_eq!(&bytes[..], &[0b1100_0000]);
    }
}

#[cfg(test)]
mod encoder_tests {
    use super::*;
    use tokio_util::codec::Encoder;

    #[test]
    fn encode_okay() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        let okay = Message::Okay;
        encoder.encode(okay, &mut bytes).ok();
        assert_eq!(&bytes[..], &[5, 0, 0, 0, MessageType::Okay as u8]);
    }

    #[test]
    fn encode_error() {
        let mut bytes = BytesMut::new();
        let error = MigrateError::Initiate;
        let mut encoder = LiveMigrationFramer {};
        encoder.encode(Message::Error(error), &mut bytes).ok();
        assert_eq!(&bytes[..5], &[13, 0, 0, 0, MessageType::Error as u8]);
        assert_eq!(&bytes[5..], br#"Initiate"#);
    }

    #[test]
    fn encode_serialized() {
        let mut bytes = BytesMut::new();
        let obj = String::from("this is an object");
        let mut encoder = LiveMigrationFramer {};
        encoder.encode(Message::Serialized(obj), &mut bytes).ok();
        assert_eq!(
            &bytes[..5],
            &[17 + 5, 0, 0, 0, MessageType::Serialized as u8]
        );
        assert_eq!(&bytes[5..], b"this is an object");
    }

    #[test]
    fn encode_empty_blob() {
        let mut bytes = BytesMut::new();
        let empty = Vec::new();
        let mut encoder = LiveMigrationFramer {};
        encoder.encode(Message::Blob(empty), &mut bytes).ok();
        assert_eq!(&bytes[..], &[5, 0, 0, 0, MessageType::Blob as u8]);
    }

    #[test]
    fn encode_blob() {
        let mut bytes = BytesMut::new();
        let empty = vec![1, 2, 3, 4];
        let mut encoder = LiveMigrationFramer {};
        encoder.encode(Message::Blob(empty), &mut bytes).ok();
        assert_eq!(
            &bytes[..],
            &[9, 0, 0, 0, MessageType::Blob as u8, 1, 2, 3, 4]
        );
    }

    #[test]
    fn encode_page() {
        let mut bytes = BytesMut::new();
        let page = [0u8; 4096];
        let mut encoder = LiveMigrationFramer {};
        encoder.encode(Message::Page(page.to_vec()), &mut bytes).ok();
        assert_eq!(
            &bytes[..5],
            [5, 0b0001_0000, 0, 0, MessageType::Page as u8]
        );
        assert!(&bytes[5..].iter().all(|&x| x == 0));
    }

    #[test]
    fn encode_mem_query() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        encoder.encode(Message::MemQuery(1, 2), &mut bytes).ok();
        assert_eq!(&bytes[..5], &[21, 0, 0, 0, MessageType::MemQuery as u8]);
        assert_eq!(&bytes[5..5 + 8], &[1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[5 + 8..], &[2, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn encode_mem_offer() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        encoder
            .encode(Message::MemOffer(0, 0x8000, vec![0b1010_0101]), &mut bytes)
            .ok();
        assert_eq!(&bytes[..5], [22, 0, 0, 0, MessageType::MemOffer as u8]);
        assert_eq!(&bytes[5..5 + 8], &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            &bytes[5 + 8..5 + 8 + 8],
            &[0, 0b1000_0000, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(&bytes[5 + 8 + 8..], &[0b1010_0101]);
    }

    #[test]
    fn encode_mem_end() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        encoder.encode(Message::MemEnd(0, 8 * 4096), &mut bytes).ok();
        assert_eq!(&bytes[..5], [21, 0, 0, 0, MessageType::MemEnd as u8]);
        assert_eq!(&bytes[5..5 + 8], &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            &bytes[5 + 8..5 + 8 + 8],
            &[0, 0b1000_0000, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn encode_mem_fetch() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        encoder
            .encode(Message::MemFetch(0, 0x4000, vec![0b0000_0101]), &mut bytes)
            .ok();
        assert_eq!(&bytes[..5], [22, 0, 0, 0, MessageType::MemFetch as u8]);
        assert_eq!(&bytes[5..5 + 8], &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[5 + 8..5 + 8 + 8], &[0, 0x40, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[5 + 8 + 8..], &[0b0000_0101]);
    }

    #[test]
    fn encode_mem_xfer() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        encoder
            .encode(Message::MemXfer(0, 0x8000, vec![0b1010_0101]), &mut bytes)
            .ok();
        assert_eq!(&bytes[..5], [22, 0, 0, 0, MessageType::MemXfer as u8]);
        assert_eq!(&bytes[5..5 + 8], &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[5 + 8..5 + 8 + 8], &[0, 0x80, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[5 + 8 + 8..], &[0b1010_0101]);
    }

    #[test]
    fn encode_mem_done() {
        let mut bytes = BytesMut::new();
        let mut encoder = LiveMigrationFramer {};
        encoder.encode(Message::MemDone, &mut bytes).ok();
        assert_eq!(&bytes[..], [5, 0, 0, 0, MessageType::MemDone as u8]);
    }
}

#[cfg(test)]
mod live_migration_decoder_tests {
    use super::*;

    #[test]
    fn get_start_end() {
        let mut bytes = BytesMut::new();
        bytes.put_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0]);
        let mut decoder = LiveMigrationFramer {};
        let (_, start, end) =
            decoder.get_start_end(bytes.remaining(), &mut bytes).unwrap();
        assert_eq!(start, 1);
        assert_eq!(end, 2);
    }

    #[test]
    fn get_bitmap_empty() {
        let mut bytes = BytesMut::new();
        let mut decoder = LiveMigrationFramer {};
        let bitmap = decoder.get_bitmap(0, &mut bytes).unwrap();
        assert_eq!(bitmap.len(), 0);
    }

    #[test]
    fn get_bitmap_exact() {
        let mut bytes = BytesMut::with_capacity(1);
        bytes.put_u8(0b1111_0000);
        let mut decoder = LiveMigrationFramer {};
        let bitmap = decoder.get_bitmap(1, &mut bytes).unwrap();
        assert_eq!(bitmap.len(), 1);
        assert_eq!(bitmap[0], 0b1111_0000);
    }
}

#[cfg(test)]
mod decoder_tests {
    use super::*;
    use std::assert_matches::assert_matches;
    use tokio_util::codec::Decoder;

    #[test]
    fn decode_bad_tag_fails() {
        let mut bytes = BytesMut::with_capacity(5);
        bytes.put_slice(&[5, 0, 0, 0, 222]);
        let mut decoder = LiveMigrationFramer {};
        assert!(decoder.decode(&mut bytes).is_err());
    }

    #[test]
    fn decode_short() {
        let mut bytes = BytesMut::with_capacity(5);
        bytes.put_slice(&[5, 0, 0]);
        let mut decoder = LiveMigrationFramer {};
        assert_matches!(decoder.decode(&mut bytes), Ok(None));
        bytes.put_slice(&[0, 0]);
        assert_matches!(decoder.decode(&mut bytes), Ok(Some(Message::Okay)));
    }

    #[test]
    fn decode_bad_length_fails() {
        let mut bytes = BytesMut::with_capacity(5);
        bytes.put_slice(&[3, 0, 0, 0, 0]);
        let mut decoder = LiveMigrationFramer {};
        assert!(decoder.decode(&mut bytes).is_err());
    }

    #[test]
    fn decode_error() {
        let mut bytes = BytesMut::with_capacity(16);
        bytes.put_slice(&[16, 0, 0, 0, MessageType::Error as u8]);
        bytes.put_slice(&br#"Http("foo")"#[..]);
        let mut decoder = LiveMigrationFramer {};
        let decoded = decoder.decode(&mut bytes);
        let expected = MigrateError::Http("foo".into());
        assert_matches!(decoded, Ok(Some(Message::Error(e))) if e == expected);
    }

    #[test]
    fn decode_two_errors() {
        let mut bytes = BytesMut::with_capacity(16 * 2);
        bytes.put_slice(&[16, 0, 0, 0, MessageType::Error as u8]);
        bytes.put_slice(&br#"Http("foo")"#[..]);
        bytes.put_slice(&[16, 0, 0, 0, MessageType::Error as u8]);
        bytes.put_slice(&br#"Http("bar")"#[..]);
        let mut decoder = LiveMigrationFramer {};
        let decoded = decoder.decode(&mut bytes);
        let expected = MigrateError::Http("foo".into());
        assert_matches!(decoded, Ok(Some(Message::Error(e))) if e == expected);
        let decoded = decoder.decode(&mut bytes);
        let expected = MigrateError::Http("bar".into());
        assert_matches!(decoded, Ok(Some(Message::Error(e))) if e == expected);
    }

    #[test]
    fn decode_blob() {
        let mut bytes = BytesMut::with_capacity(9);
        bytes.put_slice(&[9, 0, 0, 0, MessageType::Blob as u8]);
        bytes.put_slice(&b"asdf"[..]);
        let mut decoder = LiveMigrationFramer {};
        let decoded = decoder.decode(&mut bytes);
        assert_matches!(decoded, Ok(Some(Message::Blob(b))) if b == b"asdf".to_vec());
    }

    #[test]
    fn decode_page() {
        let mut bytes = BytesMut::with_capacity(5 + 4096);
        bytes.put_slice(&[5, 0x10, 0, 0, MessageType::Page as u8]);
        let page = [0u8; 4096];
        bytes.put_slice(&page[..]);
        let mut decoder = LiveMigrationFramer {};
        let decoded = decoder.decode(&mut bytes);
        assert_matches!(decoded, Ok(Some(Message::Page(p)))
            if p.iter().all(|&b| b == 0));
    }

    #[test]
    fn decode_mem_query() {
        let mut bytes = BytesMut::with_capacity(5 + 8 + 8);
        bytes.put_slice(&[5 + 8 + 8, 0, 0, 0, MessageType::MemQuery as u8]);
        bytes.put_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
        bytes.put_slice(&[2, 0, 0, 0, 0, 0, 0, 0]);
        let mut decoder = LiveMigrationFramer {};
        let decoded = decoder.decode(&mut bytes);
        assert_matches!(decoded, Ok(Some(Message::MemQuery(start, end)))
            if start == 1 && end == 2);
    }

    #[test]
    fn decode_mem_offer() {
        let mut bytes = BytesMut::with_capacity(5 + 8 + 8 + 1);
        bytes.put_slice(&[5 + 8 + 8 + 1, 0, 0, 0, MessageType::MemOffer as u8]);
        bytes.put_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.put_slice(&[0, 0x80, 0, 0, 0, 0, 0, 0]);
        bytes.put_u8(0b0000_1111);
        let mut decoder = LiveMigrationFramer {};
        let decoded = decoder.decode(&mut bytes);
        assert_matches!(decoded, Ok(Some(Message::MemOffer(start, end, v)))
            if start == 0 && end == 0x8000 && v == vec![0b0000_1111]);
    }

    #[test]
    fn decode_mem_offer_long_bitmap() {
        let mut bytes = BytesMut::with_capacity(5 + 8 + 8 + 2);
        bytes.put_slice(&[5 + 8 + 8 + 2, 0, 0, 0, MessageType::MemOffer as u8]);
        bytes.put_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.put_slice(&[0, 0x80, 0, 0, 0, 0, 0, 0]);
        bytes.put_u8(0b0000_1111);
        bytes.put_u8(0b0000_1010);
        let mut decoder = LiveMigrationFramer {};
        let decoded = decoder.decode(&mut bytes);
        assert_matches!(decoded, Ok(Some(Message::MemOffer(start, end, v)))
            if start == 0 &&
                end == 0x8000 &&
                v == vec![0b0000_1111, 0b0000_1010]);
    }

    #[test]
    fn decode_mem_end() {
        let mut bytes = BytesMut::with_capacity(5 + 8 + 8);
        bytes.put_slice(&[5 + 8 + 8, 0, 0, 0, MessageType::MemEnd as u8]);
        bytes.put_slice(&[0, 0x40, 0, 0, 0, 0, 0, 0]);
        bytes.put_slice(&[0, 0x40 + 0x80, 0, 0, 0, 0, 0, 0]);
        let mut decoder = LiveMigrationFramer {};
        let decoded = decoder.decode(&mut bytes);
        assert_matches!(decoded, Ok(Some(Message::MemEnd(start, end)))
            if start == 0x4000 && end == 0xC000);
    }

    #[test]
    fn decode_mem_fetch() {
        let mut bytes = BytesMut::with_capacity(5 + 8 + 8 + 1);
        bytes.put_slice(&[5 + 8 + 8 + 1, 0, 0, 0, MessageType::MemFetch as u8]);
        bytes.put_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.put_slice(&[0, 0x80, 0, 0, 0, 0, 0, 0]);
        bytes.put_u8(0b0000_1111);
        let mut decoder = LiveMigrationFramer {};
        let decoded = decoder.decode(&mut bytes);
        assert_matches!(decoded, Ok(Some(Message::MemFetch(start, end, v)))
            if start == 0 && end == 0x8000 && v == vec![0b0000_1111]);
    }

    #[test]
    fn decode_mem_xfer() {
        let mut bytes = BytesMut::with_capacity(5 + 8 + 8 + 1);
        bytes.put_slice(&[5 + 8 + 8 + 1, 0, 0, 0, MessageType::MemXfer as u8]);
        bytes.put_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.put_slice(&[0, 0x80, 0, 0, 0, 0, 0, 0]);
        bytes.put_u8(0b0000_1111);
        let mut decoder = LiveMigrationFramer {};
        let decoded = decoder.decode(&mut bytes);
        assert_matches!(decoded, Ok(Some(Message::MemXfer(start, end, v)))
            if start == 0 && end == 0x8000 && v == vec![0b0000_1111]);
    }

    #[test]
    fn decode_mem_done() {
        let mut bytes = BytesMut::with_capacity(5);
        bytes.put_slice(&[5, 0, 0, 0, MessageType::MemDone as u8]);
        let mut decoder = LiveMigrationFramer {};
        assert_matches!(decoder.decode(&mut bytes), Ok(Some(Message::MemDone)));
    }
}
