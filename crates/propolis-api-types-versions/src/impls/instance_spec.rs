// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Functional code for instance spec types.

use uuid::Uuid;

use crate::latest::instance_spec::SpecKey;

impl std::fmt::Display for SpecKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Uuid(uuid) => write!(f, "{uuid}"),
            Self::Name(name) => write!(f, "{name}"),
        }
    }
}

impl std::str::FromStr for SpecKey {
    type Err = core::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(s.into())
    }
}

impl From<&str> for SpecKey {
    fn from(s: &str) -> Self {
        match Uuid::parse_str(s) {
            Ok(uuid) => Self::Uuid(uuid),
            Err(_) => Self::Name(s.to_owned()),
        }
    }
}

impl From<String> for SpecKey {
    fn from(value: String) -> Self {
        match Uuid::parse_str(value.as_str()) {
            Ok(uuid) => Self::Uuid(uuid),
            Err(_) => Self::Name(value),
        }
    }
}

impl From<Uuid> for SpecKey {
    fn from(value: Uuid) -> Self {
        Self::Uuid(value)
    }
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;

    use uuid::Uuid;

    use super::SpecKey;
    use crate::latest::components::devices::QemuPvpanic;
    use crate::latest::instance_spec::Component;

    type TestMap = BTreeMap<SpecKey, Component>;

    // Verifies that UUID-type spec keys that are serialized and deserialized
    // continue to be interpreted as UUID-type spec keys.
    #[test]
    fn spec_key_uuid_roundtrip() {
        let id = Uuid::new_v4();
        let mut map = TestMap::new();
        map.insert(
            SpecKey::Uuid(id),
            Component::QemuPvpanic(QemuPvpanic { enable_isa: true }),
        );

        let ser = serde_json::to_string(&map).unwrap();
        let unser: TestMap = serde_json::from_str(&ser).unwrap();
        let key = unser.keys().next().expect("one key in the map");
        let SpecKey::Uuid(got_id) = key else {
            panic!("expected SpecKey::Uuid, got {key}");
        };

        assert_eq!(*got_id, id);
    }

    // Verifies that serializing a name-type spec key that happens to be the
    // string representation of a UUID causes the key to deserialize as a
    // UUID-type key.
    #[test]
    fn spec_key_uuid_string_deserializes_as_uuid_variant() {
        let id = Uuid::new_v4();
        let mut map = TestMap::new();
        map.insert(
            SpecKey::Name(id.to_string()),
            Component::QemuPvpanic(QemuPvpanic { enable_isa: true }),
        );

        let ser = serde_json::to_string(&map).unwrap();
        let unser: TestMap = serde_json::from_str(&ser).unwrap();
        let key = unser.keys().next().expect("one key in the map");
        let SpecKey::Uuid(got_id) = key else {
            panic!("expected SpecKey::Uuid, got {key}");
        };

        assert_eq!(*got_id, id);
    }
}
