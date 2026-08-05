//! The audit log (design §13).
//!
//! Append-only, and enforced by the absence of any `UPDATE` or `DELETE` in this
//! file rather than by a database grant: an audit log somebody can edit is a log
//! nobody can rely on after an incident.
//!
//! Its pagination cursor is its own sequence, deliberately not `events.seq`. An
//! administrative act is not something a client replays, and borrowing the stream's
//! order would put "somebody revoked a key" into everybody's backlog.

use notify_domain::{ChannelId, Principal};
use notify_ports::{AuditEntry, AuditLog, AuditQuery, Page, PortResult};
use sqlx::Row as _;
use sqlx::postgres::PgRow;

use crate::PgStore;
use crate::mapping::{cursor, fail, from_json, parse, sequence, to_json};

#[async_trait::async_trait]
impl AuditLog for PgStore {
    async fn record(&self, entry: &AuditEntry) -> PortResult<()> {
        // Mirrored to the structured logs as well as stored, so the record survives
        // the database it is written to (design §13). `audit = true` is what keeps
        // it a separate stream from the million request lines around it.
        tracing::info!(
            audit = true,
            actor = %entry.actor.reference(),
            action = entry.action,
            target = entry.target.as_deref().unwrap_or("-"),
            channel = entry.channel.as_ref().map_or("-", ChannelId::as_str),
            "administrative action"
        );

        sqlx::query(
            "INSERT INTO audit_entries (at, actor_ref, actor, action, target, channel_id, detail)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(entry.at)
        .bind(entry.actor.reference())
        .bind(to_json("audit.actor", &entry.actor)?)
        .bind(entry.action)
        .bind(entry.target.as_deref())
        .bind(entry.channel.as_ref().map(ChannelId::as_str))
        .bind(entry.detail.clone())
        .execute(self.pool())
        .await
        .map_err(|error| fail("audit", error))
        .map(|_| ())
    }

    async fn query(&self, query: &AuditQuery) -> PortResult<Page<AuditEntry>> {
        let limit = query.page.effective_limit();

        // Newest first: an incident review starts from what just happened and works
        // backwards, so the cursor walks *down* the sequence.
        let rows = sqlx::query(
            "SELECT seq, at, actor, action, target, channel_id, detail
             FROM audit_entries
             WHERE ($1::text IS NULL OR actor_ref = $1)
               AND ($2::text IS NULL OR action = $2)
               AND ($3::text IS NULL OR channel_id = $3)
               AND ($4::timestamptz IS NULL OR at >= $4)
               AND ($5::timestamptz IS NULL OR at <= $5)
               AND ($6::bigint IS NULL OR seq < $6)
             ORDER BY seq DESC
             LIMIT $7",
        )
        .bind(query.actor.as_ref().map(Principal::reference))
        .bind(query.action.as_deref())
        .bind(query.channel.as_ref().map(ChannelId::as_str))
        .bind(query.since)
        .bind(query.until)
        .bind(query.page.after.map(sequence))
        .bind(i64::from(limit) + 1)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("audit", error))?;

        page_of(rows, limit)
    }
}

/// Take the one extra row off the end and turn it into the `next` cursor.
fn page_of(mut rows: Vec<PgRow>, limit: u32) -> PortResult<Page<AuditEntry>> {
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);

    let next = has_more
        .then(|| rows.last().map(|row| row.try_get::<i64, _>("seq")))
        .flatten()
        .transpose()
        .map_err(|error| fail("audit", error))?
        .map(cursor);

    let items = rows
        .iter()
        .map(hydrate)
        .collect::<PortResult<Vec<AuditEntry>>>()?;

    Ok(match next {
        Some(next) => Page::more(items, next),
        None => Page::last(items),
    })
}

fn hydrate(row: &PgRow) -> PortResult<AuditEntry> {
    let actor: serde_json::Value = row.try_get("actor").map_err(|e| fail("audit", e))?;
    let action: String = row.try_get("action").map_err(|e| fail("audit", e))?;
    let channel: Option<String> = row.try_get("channel_id").map_err(|e| fail("audit", e))?;

    Ok(AuditEntry {
        actor: from_json::<Principal>("audit.actor", actor)?,
        // `AuditEntry::action` is a `&'static str` because the vocabulary is a
        // closed set an incident review filters on. Interning here rather than
        // leaking the `String` keeps that true on the way back out — and a value
        // this build does not know is reported rather than silently renamed.
        action: intern_action(&action)?,
        target: row.try_get("target").map_err(|e| fail("audit", e))?,
        channel: channel
            .as_deref()
            .map(|value| parse("audit.channel", value, ChannelId::parse))
            .transpose()?,
        detail: row.try_get("detail").map_err(|e| fail("audit", e))?,
        at: row.try_get("at").map_err(|e| fail("audit", e))?,
    })
}

/// The audit vocabulary, as design §13 fixes it.
///
/// A closed list rather than `String::leak`: leaking would let a malformed row grow
/// the process's memory, and silently accepting an unknown action would let a
/// review filter on a value that means nothing.
const ACTIONS: &[&str] = &[
    "apikey.created",
    "apikey.revoked",
    "channel.created",
    "channel.deleted",
    "channel.member_added",
    "channel.member_removed",
    "device.enrolled",
    "device.revoked",
    "dlq.requeued",
    // The platform role reading somebody else's channel (§5.4, T30). Membership
    // stays the one door to content, and this is the record that the door was used.
    "platform.channel.self_join",
    "service_account.created",
    "service_account.disabled",
    "trust_policy.changed",
];

fn intern_action(action: &str) -> PortResult<&'static str> {
    ACTIONS
        .iter()
        .find(|known| **known == action)
        .copied()
        .ok_or_else(|| {
            notify_ports::PortError::corrupt(
                "audit.action",
                format!("`{action}` is not in the audit vocabulary of this build"),
            )
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing assertion is a failing test")]

    use super::*;

    #[test]
    fn the_vocabulary_covers_what_the_use_cases_actually_record() {
        // These strings are written by notify-app; a mismatch would store an entry
        // that cannot be read back, which is the one failure mode an audit log must
        // not have.
        for action in [
            "apikey.created",
            "apikey.revoked",
            "channel.created",
            "channel.deleted",
            "channel.member_added",
            "channel.member_removed",
            "device.enrolled",
            "device.revoked",
        ] {
            assert!(intern_action(action).is_ok(), "{action} is not interned");
        }
    }

    #[test]
    fn an_unknown_action_is_reported_rather_than_invented() {
        let error = intern_action("apikey.exfiltrated").unwrap_err();

        assert!(matches!(error, notify_ports::PortError::Corrupt { .. }));
    }

    #[test]
    fn the_vocabulary_has_no_duplicates() {
        let mut seen = ACTIONS.to_vec();
        seen.sort_unstable();
        seen.dedup();

        assert_eq!(seen.len(), ACTIONS.len());
    }
}
