//! Functions for dealing with protocol negotiation.
//!
//! Protocols are identified by strings of the form
//! "propolis-migrate-encoding/version". During protocol negotiation, the
//! destination sends a list of protocol encodings and versions it supports. The
//! source selects a mutually-supported protocol from this list and sends it
//! back to the destination. Thereafter they communicate using the message
//! sequence specified by the version number and encoded using the encoding
//! in the string.

use std::{
    collections::BTreeSet, fmt::Display, iter::Peekable, num::ParseIntError,
    str::FromStr,
};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// Migration offers are of the form "propolis-migrate-encoding/version".
// Offer strings with multiple versions are comma-delimited.

/// The prefix to strip from a protocol offer to get the encoding and version.
const PREFIX: &str = "propolis-migrate-";

/// The separator in a protocol offer that separates the encoding from the
/// version.
const ENCODING_VERSION_SEPARATOR: char = '/';

/// The delimiter separating offers in a single string.
const DELIMITER: char = ',';

/// Errors that can arise while parsing a protocol offer string.
#[derive(Clone, Debug, Error, PartialEq, Serialize, Deserialize)]
pub enum ProtocolParseError {
    #[error("protocol string did not begin with propolis-migrate: {0}")]
    InvalidPrefix(String),

    #[error("protocol string did not have a '/' separator: {0}")]
    NoEncodingVersionSeparator(String),

    #[error("invalid encoding: {0}")]
    InvalidEncoding(String),

    #[error("failed to parse protocol version number {0}: {1}")]
    InvalidVersionNumber(String, String),

    #[error("offered protocol set contained duplicate protocol {0}")]
    DuplicateProtocolInOffer(String),
}

/// The set of permissible encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Encoding {
    /// Encode using Rust Object Notation.
    Ron,
}

impl Display for Encoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Encoding::Ron => "ron",
            }
        )
    }
}

impl FromStr for Encoding {
    type Err = ProtocolParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ron" => Ok(Encoding::Ron),
            _ => Err(ProtocolParseError::InvalidEncoding(s.to_owned())),
        }
    }
}

/// A protocol selection.
//
// N.B. The ordering of fields in this struct matters! It ensures that the
//      derived impls of PartialOrd and Ord compare versions before encodings.
//      This ensures that the negotiation process always selects the latest
//      version irrespective of whether it has the most preferable encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Protocol {
    pub version: u32,
    pub encoding: Encoding,
}

impl Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{PREFIX}{}{ENCODING_VERSION_SEPARATOR}{}",
            self.encoding, self.version
        )
    }
}

impl FromStr for Protocol {
    type Err = ProtocolParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (encoding, version) = s
            .strip_prefix(PREFIX)
            .ok_or_else(|| ProtocolParseError::InvalidPrefix(s.to_owned()))?
            .split_once(ENCODING_VERSION_SEPARATOR)
            .ok_or_else(|| {
                ProtocolParseError::NoEncodingVersionSeparator(s.to_owned())
            })?;

        let encoding = Encoding::from_str(encoding)?;
        let version = version.parse().map_err(|e: ParseIntError| {
            ProtocolParseError::InvalidVersionNumber(
                version.to_owned(),
                e.to_string(),
            )
        })?;

        Ok(Protocol { encoding, version })
    }
}

/// The set of protocols this library supports.
static PROTOCOLS: Lazy<BTreeSet<Protocol>> = Lazy::new(|| {
    build_protocol_set_from_slice(&[Protocol {
        encoding: Encoding::Ron,
        version: 0,
    }])
});

fn build_protocol_set_from_slice(slice: &[Protocol]) -> BTreeSet<Protocol> {
    let mut set = BTreeSet::new();
    for p in slice {
        let _new = set.insert(*p);
        assert!(_new);
    }
    set
}

/// Constructs a protocol offer string from a peekable protocol iterator.
fn make_protocol_offer_from_iter<
    'a,
    T: std::iter::Iterator<Item = &'a Protocol>,
>(
    mut iter: Peekable<T>,
) -> String {
    let mut s = String::new();
    while let Some(p) = iter.next() {
        s.push_str(&p.to_string());
        if iter.peek().is_some() {
            s.push(DELIMITER);
        }
    }

    s
}

/// Constructs a protocol offer string from the static supported protocol set.
pub(super) fn make_protocol_offer() -> String {
    make_protocol_offer_from_iter(PROTOCOLS.iter().peekable())
}

/// Parses an incoming protocol offer string into a set of protocol descriptors.
fn parse_protocol_offer(
    offer: &str,
) -> Result<BTreeSet<Protocol>, ProtocolParseError> {
    let mut parsed = BTreeSet::new();
    let offers = offer.split(DELIMITER);
    for o in offers {
        let protocol: Protocol = o.parse()?;
        if !parsed.insert(protocol) {
            return Err(ProtocolParseError::DuplicateProtocolInOffer(
                protocol.to_string(),
            ));
        }
    }

    Ok(parsed)
}

/// Selects the first protocol from `offered` that appears in `supported`.
fn select_compatible_protocol(
    offered: &BTreeSet<Protocol>,
    supported: &BTreeSet<Protocol>,
) -> Option<Protocol> {
    for o in offered.iter().rev() {
        if supported.contains(o) {
            return Some(*o);
        }
    }

    None
}

/// Given an incoming protocol offer string, selects a compatible protocol to
/// use.
///
/// # Return value
///
/// `Ok(Some(selection))` if a protocol was negotiated. `Ok(None)` if the offer
/// was parsed but no mutually agreeable protocol was found therein. `Err` if
/// the offered protocol string was not parseable.
pub(super) fn select_protocol_from_offer(
    offer: &str,
) -> Result<Option<Protocol>, ProtocolParseError> {
    let offered = parse_protocol_offer(offer)?;
    Ok(select_compatible_protocol(&offered, &PROTOCOLS))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROTOCOLS_V1: [Protocol; 3] = [
        Protocol { version: 2, encoding: Encoding::Ron },
        Protocol { version: 0, encoding: Encoding::Ron },
        Protocol { version: 1, encoding: Encoding::Ron },
    ];

    const PROTOCOLS_V2: [Protocol; 5] = [
        Protocol { version: 3, encoding: Encoding::Ron },
        Protocol { version: 0, encoding: Encoding::Ron },
        Protocol { version: 2, encoding: Encoding::Ron },
        Protocol { version: 4, encoding: Encoding::Ron },
        Protocol { version: 1, encoding: Encoding::Ron },
    ];

    #[test]
    fn negotiation_selects_newest_version() {
        let v1 = build_protocol_set_from_slice(&PROTOCOLS_V2);
        let v2 = build_protocol_set_from_slice(&PROTOCOLS_V1);
        let selected = select_compatible_protocol(&v1, &v2).unwrap();
        assert_eq!(selected.version, 2);
        assert_eq!(selected.encoding, Encoding::Ron);

        let selected = select_compatible_protocol(&v2, &v1).unwrap();
        assert_eq!(selected.version, 2);
        assert_eq!(selected.encoding, Encoding::Ron);
    }

    #[test]
    fn offer_string_round_trip() {
        let offer =
            make_protocol_offer_from_iter(PROTOCOLS_V2.iter().peekable());
        let set = parse_protocol_offer(&offer).unwrap();
        assert_eq!(set, build_protocol_set_from_slice(&PROTOCOLS_V2));
    }

    #[test]
    fn parse_failures() {
        assert!("not-a-prefix".parse::<Protocol>().is_err());
        assert!("propolis-migrate-ron-1".parse::<Protocol>().is_err());
        assert!("propolis-migrate-json/2".parse::<Protocol>().is_err());
        assert!("propolis-migrate-ron/version3".parse::<Protocol>().is_err());
        assert!(parse_protocol_offer(
            "propolis-migrate-ron/1,propolis-migrate-ron/1"
        )
        .is_err());
    }
}
