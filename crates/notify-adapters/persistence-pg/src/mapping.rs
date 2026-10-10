//! Row/domain conversions shared by several repositories, and the one place a cursor is minted.

use notify_domain::{Cursor, Priority};
use notify_ports::{Audience, EventPayload, PortError, PortResult};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::{Postgres, Transaction};
use time::OffsetDateTime;

/// Postgres said no; a unique-index collision becomes [`PortError::Conflict`] so use cases can recover (T16).
pub(crate) fn fail(what: &'static str, error: sqlx::Error) -> PortError {
    if let sqlx::Error::Database(database) = &error
        && database.is_unique_violation()
    {
        return PortError::Conflict { what };
    }

    PortError::unavailable("postgres", error)
}

/// Read a `jsonb` column back into the domain; failure is [`PortError::Corrupt`], since we wrote it.
pub(crate) fn from_json<T: DeserializeOwned>(
    what: &'static str,
    value: serde_json::Value,
) -> PortResult<T> {
    serde_json::from_value(value).map_err(|error| PortError::corrupt(what, error))
}

/// Render a domain value for a `jsonb` column.
pub(crate) fn to_json<T: Serialize>(
    what: &'static str,
    value: &T,
) -> PortResult<serde_json::Value> {
    serde_json::to_value(value).map_err(|error| PortError::corrupt(what, error))
}

/// Render a plain serde enum as column text, via serde so it matches the wire spelling.
pub(crate) fn enum_to_text<T: Serialize>(what: &'static str, value: &T) -> PortResult<String> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => Ok(text),
        Ok(other) => Err(PortError::corrupt(
            what,
            format!("expected a plain string, got {other}"),
        )),
        Err(error) => Err(PortError::corrupt(what, error)),
    }
}

/// Read a plain serde enum back out of a text column.
pub(crate) fn enum_from_text<T: DeserializeOwned>(what: &'static str, text: &str) -> PortResult<T> {
    serde_json::from_value(serde_json::Value::String(text.to_owned()))
        .map_err(|error| PortError::corrupt(what, format!("`{text}`: {error}")))
}

/// The sortable rank behind `?min_priority=`, derived from the domain's ordering.
pub(crate) fn priority_rank(priority: Priority) -> i16 {
    const ORDER: [Priority; 5] = [
        Priority::Min,
        Priority::Low,
        Priority::Default,
        Priority::High,
        Priority::Urgent,
    ];

    ORDER
        .iter()
        .position(|candidate| *candidate == priority)
        .and_then(|rank| i16::try_from(rank).ok())
        .unwrap_or(0)
}

/// Parse a domain value out of a text column.
pub(crate) fn parse<T, E: core::fmt::Display>(
    what: &'static str,
    text: &str,
    parser: impl FnOnce(&str) -> Result<T, E>,
) -> PortResult<T> {
    parser(text).map_err(|error| PortError::corrupt(what, format!("`{text}`: {error}")))
}

/// A cursor read out of a `bigint` column.
pub(crate) fn cursor(sequence: i64) -> Cursor {
    // Unreachable (sequence starts at 1, ADR-0028); saturate rather than panic.
    Cursor::from_sequence(u64::try_from(sequence).unwrap_or(0))
}

/// The sequence value for a `bigint` parameter.
pub(crate) fn sequence(cursor: Cursor) -> i64 {
    i64::try_from(cursor.sequence()).unwrap_or(i64::MAX)
}

/// Append one event row in the caller's transaction and return its cursor (ADR-0012).
/// The only place in this crate that allocates a cursor.
///
/// The seq derives from the transaction id (`events_next_seq()`, ADR-0028), so readers stopping at
/// `events_watermark()` never pass a row still to commit; no lock. Per notification, the order is
/// the row-lock order only if `FOR NO KEY UPDATE` is the transaction's first write.
pub(crate) async fn append_event(
    transaction: &mut Transaction<'_, Postgres>,
    audience: &Audience,
    payload: &EventPayload,
    at: OffsetDateTime,
) -> PortResult<Cursor> {
    let (audience_kind, channel_id, user_id) = match audience {
        Audience::Channel { id } => ("channel", Some(id.to_string()), None),
        Audience::User { id } => ("user", None, Some(id.to_string())),
    };

    let sequence: i64 = sqlx::query_scalar(
        "INSERT INTO events
             (audience_kind, channel_id, user_id, recorded_at, kind, notification_id, payload)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING seq",
    )
    .bind(audience_kind)
    .bind(channel_id)
    .bind(user_id)
    .bind(at)
    .bind(payload.kind().as_str())
    .bind(payload.notification_id().map(|id| id.to_string()))
    .bind(to_json("event.payload", payload)?)
    .fetch_one(&mut **transaction)
    .await
    .map_err(|error| fail("event", error))?;

    Ok(cursor(sequence))
}

/// Events one transaction may append: the seq keeps 16 bits for them below the txid (ADR-0028).
pub(crate) const EVENTS_PER_TRANSACTION: u32 = 65_536;

/// Append an "expired unseen" row: it keeps the cursor order unbroken but is never replayed,
/// only counted (design §7.2bis, T9).
pub(crate) const EXPIRED_UNSEEN: &str = "expired_unseen";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing assertion is a failing test")]

    use notify_domain::{State, Status};

    use super::*;

    #[test]
    fn a_stored_enum_is_spelled_the_way_the_wire_spells_it() {
        assert_eq!(enum_to_text("state", &State::Working).unwrap(), "working");
        assert_eq!(
            enum_to_text("status", &Status::Cancelled).unwrap(),
            "cancelled"
        );
        assert_eq!(
            enum_from_text::<State>("closed", "closed").unwrap(),
            State::Closed
        );
    }

    #[test]
    fn a_column_holding_something_no_current_type_accepts_is_our_fault_not_the_callers() {
        let error = enum_from_text::<State>("state", "levitating").unwrap_err();

        assert!(matches!(error, PortError::Corrupt { .. }));
        assert!(!error.is_retryable(), "retrying a bad row is a busy loop");
    }

    #[test]
    fn priority_ranks_in_the_order_the_domain_declares() {
        let ranks: Vec<_> = [
            Priority::Min,
            Priority::Low,
            Priority::Default,
            Priority::High,
            Priority::Urgent,
        ]
        .into_iter()
        .map(priority_rank)
        .collect();

        assert_eq!(ranks, [0, 1, 2, 3, 4]);
        assert!(
            ranks.windows(2).all(|pair| pair[0] < pair[1]),
            "`?min_priority=high` is a range scan on this number"
        );
    }

    #[test]
    fn a_cursor_survives_the_round_trip_through_a_bigint() {
        let original = Cursor::from_sequence(4_294_967_297);
        assert_eq!(cursor(sequence(original)), original);
    }
}
