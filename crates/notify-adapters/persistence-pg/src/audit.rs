//! The audit log (design §13): append-only — a trigger (ADR-0026) and no `UPDATE`/`DELETE` here;
//! the one deletion is `wnk_audit_purge` (ADR-0027). Its own sequence rather than `events.seq`
//! so admin acts never reach client backlogs.

use notify_domain::ChannelId;
use notify_ports::{AuditActor, AuditEntry, AuditLog, AuditQuery, Page, PortResult};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{cursor, fail, from_json, parse, sequence, to_json};

#[async_trait::async_trait]
impl AuditLog for PgStore {
    async fn record(&self, entry: &AuditEntry) -> PortResult<()> {
        // Mirrored to logs (`audit = true`) so the record survives the database (§13).
        tracing::info!(
            audit = true,
            actor = %entry.actor.reference(),
            action = entry.action.as_str(),
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
        .bind(entry.action.as_str())
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

        // Newest first: the cursor walks down the sequence.
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
        .bind(query.actor.as_ref().map(AuditActor::reference))
        .bind(query.action.as_deref())
        .bind(query.channel.as_ref().map(ChannelId::as_str))
        .bind(query.since)
        .bind(query.until)
        .bind(query.page.after.map(sequence))
        .bind(i64::from(limit).saturating_add(1))
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("audit", error))?;

        page_of(rows, limit)
    }

    async fn purge_older_than(&self, cutoff: OffsetDateTime, batch: u32) -> PortResult<u64> {
        // The function is the whole decision — the floor, the batch, the
        // `audit.purged` entry — so a caller here cannot get any of it wrong.
        // `tests/audit_is_append_only.rs` holds this to being its only caller.
        let purged: i64 = sqlx::query_scalar("SELECT wnk_audit_purge($1, $2)")
            .bind(cutoff)
            .bind(i32::try_from(batch).unwrap_or(i32::MAX))
            .fetch_one(self.pool())
            .await
            .map_err(|error| fail("audit", error))?;

        Ok(u64::try_from(purged).unwrap_or(0))
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
        actor: from_json::<AuditActor>("audit.actor", actor)?,
        // Interned against the closed vocabulary; an unknown value is reported, not renamed.
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

/// Intern against the port's `notify_ports::AUDIT_ACTIONS`, the same list the writers use.
fn intern_action(action: &str) -> PortResult<notify_ports::AuditAction> {
    notify_ports::AuditAction::parse(action).ok_or_else(|| {
        notify_ports::PortError::corrupt(
            "audit.action",
            format!("`{action}` is not in the audit vocabulary of this build"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_in_the_vocabulary_interns() {
        // Walks the shared vocabulary, so there is no second copy to drift.
        for action in notify_ports::AUDIT_ACTIONS {
            assert!(
                intern_action(action.as_str()).is_ok(),
                "{action} is not interned"
            );
        }
    }

    #[test]
    fn an_unknown_action_is_reported_rather_than_invented() {
        let error = intern_action("apikey.exfiltrated").unwrap_err();

        assert!(matches!(error, notify_ports::PortError::Corrupt { .. }));
    }

    #[test]
    fn the_vocabulary_has_no_duplicates() {
        let mut seen = notify_ports::AUDIT_ACTIONS.to_vec();
        seen.sort_unstable();
        seen.dedup();

        assert_eq!(seen.len(), notify_ports::AUDIT_ACTIONS.len());
    }
}
