// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI name encoding utilities.
//!
//! See ACPI Specification 6.4, Section 20.2.2 for Name Objects Encoding.

use super::opcodes::{DUAL_NAME_PREFIX, MULTI_NAME_PREFIX, ROOT_PREFIX};

pub const MAX_NAME_SEGS: usize = 255;

pub const NAME_SEG_SIZE: usize = 4;

/// Encode a 4-character ACPI name segment, padding shorter names with '_'.
pub fn encode_name_seg(name: &str) -> [u8; NAME_SEG_SIZE] {
    assert!(name.len() <= NAME_SEG_SIZE, "name segment too long: {}", name);
    assert!(!name.is_empty(), "name segment cannot be empty");

    let bytes = name.as_bytes();

    let first = bytes[0];
    assert!(
        first.is_ascii_uppercase() || first == b'_',
        "invalid first character in name segment: {}",
        name
    );

    for &c in &bytes[1..] {
        assert!(
            c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_',
            "invalid character in name segment: {}",
            name
        );
    }

    let mut seg = [b'_'; NAME_SEG_SIZE];
    seg[..bytes.len()].copy_from_slice(bytes);
    seg
}

/// Encode an ACPI name path (e.g. "\\_SB_.PCI0") into the buffer.
pub fn encode_name_string(path: &str, buf: &mut Vec<u8>) {
    let mut chars = path.chars().peekable();

    if chars.peek() == Some(&'\\') {
        buf.push(ROOT_PREFIX);
        chars.next();
    }

    while chars.peek() == Some(&'^') {
        buf.push(super::opcodes::PARENT_PREFIX);
        chars.next();
    }

    let remaining: String = chars.collect();
    if remaining.is_empty() {
        return;
    }

    let segments: Vec<&str> = remaining.split('.').collect();
    assert!(
        segments.len() <= MAX_NAME_SEGS,
        "too many name segments: {}",
        segments.len()
    );

    match segments.len() {
        0 => {}
        1 => {
            let seg = encode_name_seg(segments[0]);
            buf.extend_from_slice(&seg);
        }
        2 => {
            buf.push(DUAL_NAME_PREFIX);
            for s in &segments {
                let seg = encode_name_seg(s);
                buf.extend_from_slice(&seg);
            }
        }
        n => {
            buf.push(MULTI_NAME_PREFIX);
            buf.push(n as u8);
            for s in &segments {
                let seg = encode_name_seg(s);
                buf.extend_from_slice(&seg);
            }
        }
    }
}

/// Encode an EISA ID string (e.g. "PNP0A08") into a 32-bit value.
pub fn encode_eisaid(id: &str) -> u32 {
    assert_eq!(id.len(), 7, "EISA ID must be exactly 7 characters: {}", id);

    let bytes = id.as_bytes();

    for (i, &c) in bytes[0..3].iter().enumerate() {
        assert!(
            c.is_ascii_uppercase(),
            "EISA ID manufacturer code must be A-Z at position {}: {}",
            i,
            id
        );
    }

    let c1 = bytes[0] - b'A' + 1;
    let c2 = bytes[1] - b'A' + 1;
    let c3 = bytes[2] - b'A' + 1;
    let mfg = ((c1 as u16) << 10) | ((c2 as u16) << 5) | (c3 as u16);

    let product =
        u16::from_str_radix(&id[3..7], 16).expect("invalid hex in EISA ID");

    let mfg_bytes = mfg.to_be_bytes();
    let product_bytes = product.to_be_bytes();

    u32::from_le_bytes([
        mfg_bytes[0],
        mfg_bytes[1],
        product_bytes[0],
        product_bytes[1],
    ])
}

/// EISA ID wrapper that implements AmlWriter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EisaId(pub u32);

impl EisaId {
    pub fn from_str(id: &str) -> Self {
        Self(encode_eisaid(id))
    }
}

/// UUID byte size.
pub const UUID_SIZE: usize = 16;

/// Encode a UUID string into ACPI ToUUID format at compile time.
///
/// UUID format: "XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX"
///
/// ACPI ToUUID uses mixed-endian encoding:
/// - First 3 groups (8-4-4 hex digits): little-endian
/// - Last 2 groups (4-12 hex digits): big-endian
pub const fn encode_uuid(uuid: &str) -> [u8; UUID_SIZE] {
    let b = uuid.as_bytes();
    assert!(b.len() == 36, "UUID must be 36 characters");
    assert!(
        b[8] == b'-' && b[13] == b'-' && b[18] == b'-' && b[23] == b'-',
        "UUID must have dashes at positions 8, 13, 18, 23"
    );

    const fn hex(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'A'..=b'F' => c - b'A' + 10,
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("invalid hex"),
        }
    }

    const fn byte(b: &[u8], i: usize) -> u8 {
        (hex(b[i]) << 4) | hex(b[i + 1])
    }

    [
        byte(b, 6),
        byte(b, 4),
        byte(b, 2),
        byte(b, 0),
        byte(b, 11),
        byte(b, 9),
        byte(b, 16),
        byte(b, 14),
        byte(b, 19),
        byte(b, 21),
        byte(b, 24),
        byte(b, 26),
        byte(b, 28),
        byte(b, 30),
        byte(b, 32),
        byte(b, 34),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_seg_encoding() {
        assert_eq!(encode_name_seg("_SB_"), *b"_SB_");
        assert_eq!(encode_name_seg("PCI0"), *b"PCI0");
        assert_eq!(encode_name_seg("A"), *b"A___");
        assert_eq!(encode_name_seg("AB"), *b"AB__");
    }

    #[test]
    #[should_panic(expected = "name segment too long")]
    fn name_seg_rejects_long() {
        encode_name_seg("TOOLONG");
    }

    #[test]
    #[should_panic(expected = "invalid first character")]
    fn name_seg_rejects_leading_digit() {
        encode_name_seg("1BAD");
    }

    #[test]
    fn name_string_encoding() {
        let mut buf = Vec::new();
        encode_name_string("_SB_", &mut buf);
        assert_eq!(buf, b"_SB_");

        buf.clear();
        encode_name_string("\\_SB_", &mut buf);
        assert_eq!(buf, vec![ROOT_PREFIX, b'_', b'S', b'B', b'_']);

        buf.clear();
        encode_name_string("_SB_.PCI0", &mut buf);
        assert_eq!(buf[0], DUAL_NAME_PREFIX);

        buf.clear();
        encode_name_string("_SB_.PCI0.ISA_", &mut buf);
        assert_eq!(buf[0], MULTI_NAME_PREFIX);
        assert_eq!(buf[1], 3);
    }

    #[test]
    fn eisaid_encoding() {
        assert_eq!(encode_eisaid("PNP0A08"), 0x080AD041);
        assert_eq!(encode_eisaid("PNP0A03"), 0x030AD041);
        assert_eq!(encode_eisaid("PNP0501"), 0x0105D041);
        assert_eq!(EisaId::from_str("PNP0A08").0, 0x080AD041);
    }

    #[test]
    #[should_panic(expected = "EISA ID manufacturer code must be A-Z")]
    fn eisaid_rejects_lowercase() {
        encode_eisaid("pnp0A08");
    }

    #[test]
    fn uuid_encoding() {
        let uuid = encode_uuid("33DB4D5B-1FF7-401C-9657-7441C03DD766");
        assert_eq!(
            uuid,
            [
                0x5B, 0x4D, 0xDB, 0x33, 0xF7, 0x1F, 0x1C, 0x40, 0x96, 0x57,
                0x74, 0x41, 0xC0, 0x3D, 0xD7, 0x66,
            ]
        );
    }
}
