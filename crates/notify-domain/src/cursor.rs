//! The sync cursor: one total order shared by `GET /sync`, the stream `id:` and
//! `Last-Event-ID` (ADR-0001, ADR-0012). Opaque by contract (encoding, not encryption).

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{DomainError, Result};

/// A position in the global event sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
// Documented as an opaque string so clients don't do arithmetic on it.
#[cfg_attr(
    feature = "openapi",
    schema(value_type = String, example = "evt_00000000000042")
)]
pub struct Cursor(u64);

impl Cursor {
    /// The textual prefix, so a cursor is recognisable in a log line.
    pub const PREFIX: &'static str = "evt_";

    /// Before the first event: syncing from here gets everything still retained.
    pub const START: Self = Self(0);

    /// Wrap a sequence value from storage.
    #[must_use]
    pub const fn from_sequence(value: u64) -> Self {
        Self(value)
    }

    /// The underlying sequence value, for the adapter that owns the table.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.0
    }

    /// The next position, saturating rather than wrapping.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Zero-padded so cursors sort the same as strings and as numbers.
        write!(f, "{}{:016x}", Self::PREFIX, self.0)
    }
}

impl FromStr for Cursor {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self> {
        let malformed = DomainError::Malformed {
            field: "cursor",
            reason: "not a cursor issued by this server",
        };
        let body = raw.strip_prefix(Self::PREFIX).ok_or(malformed.clone())?;
        u64::from_str_radix(body, 16)
            .map(Self)
            .map_err(|_| malformed)
    }
}

impl Serialize for Cursor {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Cursor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    #[test]
    fn cursors_round_trip_through_their_rendered_form() {
        let cursor = Cursor::from_sequence(4_521);
        assert_eq!(cursor.to_string(), "evt_00000000000011a9");
        assert_eq!(cursor.to_string().parse::<Cursor>().unwrap(), cursor);
    }

    #[test]
    fn cursors_order_the_same_as_text_and_as_numbers() {
        let earlier = Cursor::from_sequence(9);
        let later = Cursor::from_sequence(4_521);
        assert!(earlier < later);
        assert!(earlier.to_string() < later.to_string());
    }

    #[test]
    fn a_foreign_cursor_is_refused() {
        for bad in ["4521", "evt_zzzz", "ntf_01J", ""] {
            assert!(bad.parse::<Cursor>().is_err(), "`{bad}` parsed");
        }
    }

    #[test]
    fn the_start_cursor_precedes_every_event() {
        assert_eq!(Cursor::START.sequence(), 0);
        assert!(Cursor::START < Cursor::START.next());
    }
}
