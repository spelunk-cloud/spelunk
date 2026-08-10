//! Opaque identity for a memory entry as seen through [`MemoryBackend`].
//!
//! A local SQLite store mints `INTEGER PRIMARY KEY` rowids and the OSS team
//! server mirrors that shape, but the hosted cloud API mints UUIDs. The trait
//! therefore cannot thread `i64` and still address every backend, so identity
//! is carried as an opaque string and narrowed back to `i64` only by the
//! backends that genuinely have one.
//!
//! [`MemoryBackend`]: crate::storage::backend::MemoryBackend

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::fmt;
use std::str::FromStr;

/// The identity of a memory entry: an opaque, backend-minted token.
///
/// Ordering is lexicographic over the raw token and carries no meaning across
/// backends; it exists so callers can put ids in a `BTreeMap` for stable
/// output, not so they can infer recency.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NoteId(String);

impl NoteId {
    pub fn from_i64(id: i64) -> Self {
        NoteId(id.to_string())
    }

    /// The `i64` this id denotes, or `None` when it is not numeric.
    ///
    /// Backends keyed by a SQLite rowid call this to narrow an id they were
    /// handed; a `None` is a caller error (a cloud UUID aimed at a local
    /// store), not a corrupt row, so it deserves an actionable message rather
    /// than a panic.
    pub fn as_i64(&self) -> Option<i64> {
        self.0.parse().ok()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NoteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<i64> for NoteId {
    fn from(id: i64) -> Self {
        NoteId::from_i64(id)
    }
}

/// Rejects only the empty string. Every other token is a valid opaque id:
/// this type cannot know which backend will be asked to resolve it, so it
/// must not impose that backend's shape at parse time.
impl FromStr for NoteId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err("memory entry id must not be empty".to_string());
        }
        Ok(NoteId(s.to_string()))
    }
}

/// Numeric ids serialize as JSON numbers, everything else as a string.
///
/// `spelunk memory list --format json` emitted `"id": 42` before identity
/// became opaque, and scripts parse that. Unconditionally quoting would break
/// them for the local and team-server populations, which is every existing
/// user, to describe a cloud UUID none of them have.
impl Serialize for NoteId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.as_i64() {
            Some(n) => s.serialize_i64(n),
            None => s.serialize_str(&self.0),
        }
    }
}

/// Accepts a JSON number or string, mirroring [`Serialize`]. The team server
/// sends `"id": 42`; cloud-api sends `"id": "<uuid>"`.
impl<'de> Deserialize<'de> for NoteId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;

        impl de::Visitor<'_> for V {
            type Value = NoteId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a memory entry id (integer or string)")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<NoteId, E> {
                Ok(NoteId::from_i64(v))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<NoteId, E> {
                Ok(NoteId(v.to_string()))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<NoteId, E> {
                NoteId::from_str(v).map_err(E::custom)
            }
        }

        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_an_i64() {
        let id = NoteId::from_i64(42);
        assert_eq!(id.as_i64(), Some(42));
        assert_eq!(id.to_string(), "42");
    }

    #[test]
    fn a_uuid_has_no_i64_narrowing() {
        let id: NoteId = "9f1c2d3e-4a5b-6c7d-8e9f-0a1b2c3d4e5f".parse().unwrap();
        assert_eq!(id.as_i64(), None);
        assert_eq!(id.as_str(), "9f1c2d3e-4a5b-6c7d-8e9f-0a1b2c3d4e5f");
    }

    #[test]
    fn negative_rowids_narrow_too() {
        assert_eq!(NoteId::from_i64(-7).as_i64(), Some(-7));
    }

    #[test]
    fn empty_is_the_only_rejected_token() {
        assert!("".parse::<NoteId>().is_err());
        assert!("not-a-number".parse::<NoteId>().is_ok());
        assert!(" ".parse::<NoteId>().is_ok());
    }

    // A numeric id must stay a JSON number: `--format json` consumers predate
    // opaque identity and parse `"id": 42`.
    #[test]
    fn numeric_ids_serialize_as_json_numbers() {
        assert_eq!(serde_json::to_string(&NoteId::from_i64(42)).unwrap(), "42");
    }

    #[test]
    fn non_numeric_ids_serialize_as_json_strings() {
        let id: NoteId = "abc-123".parse().unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"abc-123\"");
    }

    #[test]
    fn deserializes_from_either_wire_shape() {
        assert_eq!(
            serde_json::from_str::<NoteId>("42").unwrap(),
            NoteId::from_i64(42)
        );
        assert_eq!(
            serde_json::from_str::<NoteId>("\"a-uuid\"").unwrap(),
            "a-uuid".parse::<NoteId>().unwrap()
        );
    }

    #[test]
    fn serialize_deserialize_is_lossless_for_both_shapes() {
        for id in [NoteId::from_i64(7), "7a".parse().unwrap()] {
            let json = serde_json::to_string(&id).unwrap();
            assert_eq!(serde_json::from_str::<NoteId>(&json).unwrap(), id);
        }
    }
}
