// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Functions for dealing with protocol negotiation.
//!
//! Protocols are identified by strings of the form
//! "propolis-migrate-encoding/version". During protocol negotiation, the
//! destination sends a list of protocol encodings and versions it supports. The
//! source selects a mutually-supported protocol from this list and sends it
//! back to the destination. Thereafter they communicate using the message
//! sequence specified by the version number and encoded using the encoding
//! in the string.

use std::{fmt::Display, iter::Peekable, num::ParseIntError, str::FromStr};

use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use strum::{EnumIter, IntoEnumIterator};
use thiserror::Error;

/// The complete set of protocols supported by this version of the migration
/// library.
#[derive(Debug, Clone, Copy, EnumIter)]
pub enum Protocol {
    RonV0,
}

impl Protocol {
    /// Yields the offer string for this protocol variant. This can be sent to
    /// a migration counterpart to offer this protocol version.
    pub fn offer_string(&self) -> String {
        ProtocolParts::from(*self).offer_string()
    }
}

impl TryFrom<ProtocolParts> for Protocol {
    type Error = anyhow::Error;

    fn try_from(value: ProtocolParts) -> Result<Self, Self::Error> {
        let protocol = match value {
            ProtocolParts { encoding: Encoding::Ron, version: 0 } => {
                Self::RonV0
            }
            _ => anyhow::bail!(format!(
                "no protocol matching definition: {value:?}"
            )),
        };

        Ok(protocol)
    }
}

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
enum Encoding {
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
struct ProtocolParts {
    version: u32,
    encoding: Encoding,
}

impl ProtocolParts {
    fn offer_string(&self) -> String {
        format!(
            "{}{}{}{}",
            PREFIX, self.encoding, ENCODING_VERSION_SEPARATOR, self.version
        )
    }
}

impl From<Protocol> for ProtocolParts {
    fn from(value: Protocol) -> Self {
        match value {
            Protocol::RonV0 => {
                ProtocolParts { version: 0, encoding: Encoding::Ron }
            }
        }
    }
}

impl FromStr for ProtocolParts {
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

        Ok(ProtocolParts { encoding, version })
    }
}

lazy_static! {
    static ref PROTOCOL_PARTS: Vec<ProtocolParts> =
        Protocol::iter().map(ProtocolParts::from).collect();
}

/// Constructs a protocol offer string from a peekable protocol iterator.
fn make_protocol_offers_from_parts<
    T: std::iter::Iterator<Item = ProtocolParts>,
>(
    mut iter: Peekable<T>,
) -> String {
    let mut s = String::new();
    while let Some(p) = iter.next() {
        s.push_str(&p.offer_string());
        if iter.peek().is_some() {
            s.push(DELIMITER);
        }
    }

    s
}

/// Constructs a protocol offer string from the static supported protocol set.
pub(super) fn make_protocol_offer() -> String {
    make_protocol_offers_from_parts(
        Protocol::iter().map(ProtocolParts::from).peekable(),
    )
}

/// Parses an incoming protocol offer string into a set of protocol descriptors.
fn parse_protocol_offer(
    offer: &str,
) -> Result<Vec<ProtocolParts>, ProtocolParseError> {
    let mut parsed = Vec::new();
    let offers = offer.split(DELIMITER);
    for o in offers {
        let protocol: ProtocolParts = o.parse()?;
        if parsed.contains(&protocol) {
            return Err(ProtocolParseError::DuplicateProtocolInOffer(
                protocol.offer_string(),
            ));
        }

        parsed.push(protocol);
    }

    parsed.sort_unstable();
    Ok(parsed)
}

/// Selects the first protocol from `offered` that appears in `supported`.
/// The caller must ensure that `offered` is sorted ascending.
fn select_compatible_protocol(
    offered: &[ProtocolParts],
    supported: &[ProtocolParts],
) -> Option<ProtocolParts> {
    assert!(offered.windows(2).all(|subslice| subslice[0] <= subslice[1]));

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
    Ok(select_compatible_protocol(&offered, &PROTOCOL_PARTS).map(|parts| {
        parts.try_into().expect(
            "compatible protocol strings should have a Protocol variant",
        )
    }))
}

#[cfg(test)]
mod test {
    use super::*;

    // N.B. The test protocol lists are sorted by version to meet the
    //      requirements of `select_compatible_protocol`.
    const PROTOCOLS_V1: [ProtocolParts; 3] = [
        ProtocolParts { version: 0, encoding: Encoding::Ron },
        ProtocolParts { version: 1, encoding: Encoding::Ron },
        ProtocolParts { version: 2, encoding: Encoding::Ron },
    ];

    const PROTOCOLS_V2: [ProtocolParts; 5] = [
        ProtocolParts { version: 0, encoding: Encoding::Ron },
        ProtocolParts { version: 1, encoding: Encoding::Ron },
        ProtocolParts { version: 2, encoding: Encoding::Ron },
        ProtocolParts { version: 3, encoding: Encoding::Ron },
        ProtocolParts { version: 4, encoding: Encoding::Ron },
    ];

    #[test]
    fn negotiation_selects_newest_version() {
        let selected =
            select_compatible_protocol(&PROTOCOLS_V1, &PROTOCOLS_V2).unwrap();
        assert_eq!(selected.version, 2);
        assert_eq!(selected.encoding, Encoding::Ron);

        let selected =
            select_compatible_protocol(&PROTOCOLS_V1, &PROTOCOLS_V2).unwrap();
        assert_eq!(selected.version, 2);
        assert_eq!(selected.encoding, Encoding::Ron);
    }

    #[test]
    fn parse_sorts_offered_protocols() {
        let parts_str = "propolis-migrate-ron/42,\
            propolis-migrate-ron/65,\
            propolis-migrate-ron/0,\
            propolis-migrate-ron/27";

        let offered = parse_protocol_offer(parts_str).unwrap();
        let expected_versions = [0, 27, 42, 65];
        assert_eq!(
            offered.iter().map(|parts| parts.version).collect::<Vec<u32>>(),
            expected_versions
        );
    }

    #[test]
    fn offer_string_round_trip() {
        let offer = make_protocol_offers_from_parts(
            PROTOCOLS_V2.iter().map(Clone::clone).peekable(),
        );
        let set = parse_protocol_offer(&offer).unwrap();
        assert_eq!(set, PROTOCOLS_V2);
    }

    #[test]
    fn parse_failures() {
        assert!("not-a-prefix".parse::<ProtocolParts>().is_err());
        assert!("propolis-migrate-ron-1".parse::<ProtocolParts>().is_err());
        assert!("propolis-migrate-json/2".parse::<ProtocolParts>().is_err());
        assert!("propolis-migrate-ron/version3"
            .parse::<ProtocolParts>()
            .is_err());

        assert!(parse_protocol_offer(
            "propolis-migrate-ron/1,propolis-migrate-ron/1"
        )
        .is_err());
    }
}
