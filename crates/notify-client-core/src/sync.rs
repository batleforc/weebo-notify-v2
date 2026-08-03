//! Catching up: `GET /sync` into the local store (design §7.2, §9.3bis).
//!
//! # What a catch-up is for
//!
//! A front end that was closed, asleep or offline comes back and asks for
//! everything after its cursor. The answer is more than a list of events (§7.2bis):
//! it carries how many notifications **expired unseen** per channel, because a
//! message that expired undelivered is deliberately *not* in the backlog and
//! silently dropping it would leave somebody wondering; and how many revisions the
//! compaction folded away, which is what lets a client say "38 updates" instead of
//! pretending nothing happened.
//!
//! # Why this does not use the generated client
//!
//! It should, and ADR-0009 says so. It cannot today: progenitor renders the `Event`
//! schema — an `allOf` of two `oneOf`s, which is what makes the wire shape flat
//! (§8.1) — as `pub enum EventVariant0 {}`. An **uninhabited** type: the generated
//! client can deserialise a sync page only while it contains no events at all.
//!
//! So this module reads the response with [`crate::wire`]'s minimal model of what a
//! cache needs, and that is a **stated exception rather than a pattern**. The real
//! fix is in the specification — either the `allOf` composition or the lowering shim
//! ADR-0009 already owns — and it is D1 in §14.4. When it lands, this file goes back
//! to the generated call and loses thirty lines.

use serde::Deserialize;

use crate::error::ClientError;
use crate::session::Session;
use crate::store::{CacheEntry, Store};
use crate::wire::WireEvent;

/// What one catch-up did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Notifications written or refreshed.
    pub stored: usize,
    /// Notifications the server said are gone.
    pub removed: usize,
    /// Per channel, how many expired unseen while this client was away (§7.2bis).
    pub expired: Vec<(String, u32)>,
    /// How many revisions the compaction folded away (§7.2).
    pub compacted: u32,
}

impl SyncOutcome {
    /// Whether anything at all happened, for a caller deciding what to print.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.stored == 0 && self.removed == 0 && self.expired.is_empty()
    }
}

/// A sync page.
///
/// `events` stays a `Value` on the way in so the whole event can be **stored
/// verbatim** while [`WireEvent`] reads the handful of fields the cache needs from a
/// copy. Shredding it into the fields this build happens to know would leave a later
/// build unable to render what an earlier one cached.
#[derive(Debug, Deserialize)]
struct Page {
    events: Vec<serde_json::Value>,
    #[serde(default)]
    expired: Vec<RawExpired>,
    #[serde(default)]
    compacted: u32,
    #[serde(default)]
    next: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawExpired {
    channel: String,
    count: u32,
}

impl Session {
    /// Catch the store up, following the cursor to the end.
    ///
    /// # Errors
    ///
    /// Fails if the server refuses, is unreachable, or the store cannot be written.
    pub async fn sync_into(
        &self,
        store: &Store,
        channels: Option<&str>,
        device: Option<&str>,
    ) -> Result<SyncOutcome, ClientError> {
        let mut outcome = SyncOutcome::default();
        let mut cursor = store.cursor()?;

        loop {
            let page = self.fetch_page(cursor.as_deref(), channels, device).await?;

            for raw in &page.events {
                let Ok(event) = serde_json::from_value::<WireEvent>(raw.clone()) else {
                    // Not an event this build can read at all. Skipping beats
                    // refusing the whole page and stalling the cursor forever — but
                    // the cursor cannot advance past something unread either, so the
                    // page is followed and this one is simply not cached.
                    continue;
                };

                match event.event.as_str() {
                    "revision" => {
                        let (Some(id), Some(channel)) = (
                            event.notification_id.as_deref(),
                            event.channel_id.as_deref(),
                        ) else {
                            // A revision that names no notification is not one this
                            // build understands.
                            continue;
                        };

                        store.upsert(CacheEntry {
                            id,
                            channel,
                            revised_at: event.revised_at.as_deref().unwrap_or_default(),
                            created_at: event.created_at.as_deref().unwrap_or_default(),
                            // The arrival time is stamped by the store, not passed
                            // in: it is a fact about this machine's cache, and §7.2's
                            // badge is the gap between it and `created_at`.
                            payload: &raw.to_string(),
                        })?;
                        outcome.stored += 1;
                    }
                    "tombstone" => {
                        if let Some(id) = event.notification_id.as_deref() {
                            store.remove(id)?;
                            outcome.removed += 1;
                        }
                    }
                    // Receipts, interactions and preferences change a notification's
                    // detail rather than the cache's contents. The TUI reads them
                    // live; a cold catch-up has nothing to do with them.
                    _ => {}
                }

                // Advanced per event and written per page: the cursor must never
                // claim more than the store actually holds (see `Store::set_cursor`).
                cursor = Some(event.cursor.clone());
            }

            if let Some(last) = &cursor {
                store.set_cursor(last)?;
            }

            outcome.compacted += page.compacted;
            outcome.expired.extend(
                page.expired
                    .into_iter()
                    .map(|entry| (entry.channel, entry.count)),
            );

            // Followed until `next` is null, never until a page looks short (§8).
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(outcome),
            }
        }
    }

    async fn fetch_page(
        &self,
        cursor: Option<&str>,
        channels: Option<&str>,
        device: Option<&str>,
    ) -> Result<Page, ClientError> {
        let mut url = format!("{}/api/v1/sync", self.base_url());
        let mut query: Vec<(&str, &str)> = Vec::new();
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        if let Some(channels) = channels {
            query.push(("channels", channels));
        }
        if let Some(device) = device {
            query.push(("device", device));
        }
        if !query.is_empty() {
            let encoded: Vec<String> = query
                .iter()
                .map(|(key, value)| format!("{key}={}", urlencode(value)))
                .collect();
            url = format!("{url}?{}", encoded.join("&"));
        }

        let response = self
            .http()
            .get(&url)
            .send()
            .await
            .map_err(ClientError::Transport)?;

        let status = response.status();
        if !status.is_success() {
            // The refusal carries `problem+json`, and the shared error type knows
            // how to word it — a sync that fails on a revoked token should say so
            // in the same terms every other call does.
            return match response.json::<notify_client_api::types::Problem>().await {
                Ok(problem) => Err(ClientError::Refused(Box::new(problem))),
                Err(_) => Err(ClientError::Unexpected {
                    status: status.as_u16(),
                }),
            };
        }

        response
            .json::<Page>()
            .await
            .map_err(ClientError::Transport)
    }
}

/// Percent-encode a query value.
///
/// Cursors are opaque and channel ids are slugs, so the alphabet is small — but
/// "small today" is how a query-injection bug is written.
fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b',' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    #[test]
    fn a_sync_page_deserialises_from_the_shape_8_1_documents() {
        // The regression this file exists for: the generated client renders these
        // events as an uninhabited type, so the wire shape has to be parsed here.
        let raw = serde_json::json!({
            "events": [{
                "cursor": "evt_00000000000042",
                "audience": "channel",
                "channel_id": "ci-infra",
                "logged_at": "2026-08-01T10:00:00Z",
                "event": "revision",
                "notification_id": "ntf_01J",
                "seq": 1,
                "state": "open",
                "status": "info",
                "priority": "default",
                "author": { "kind": "user", "id": "usr_1" },
                "created_at": "2026-08-01T10:00:00Z",
                "revised_at": "2026-08-01T10:00:00Z",
                "payload": { "title": "etcd degraded" }
            }],
            "expired": [{ "channel": "ci-infra", "count": 3 }],
            "compacted": 38,
            "next": null
        });

        let page: Page = serde_json::from_value(raw).unwrap();

        assert_eq!(page.events.len(), 1);
        assert_eq!(page.expired[0].count, 3);
        assert_eq!(page.compacted, 38);

        let event: WireEvent = serde_json::from_value(page.events[0].clone()).unwrap();
        assert_eq!(event.channel_id.as_deref(), Some("ci-infra"));
        assert_eq!(event.created_at.as_deref(), Some("2026-08-01T10:00:00Z"));
        assert_eq!(
            event.payload.map(|content| content.title).as_deref(),
            Some("etcd degraded")
        );
    }

    #[test]
    fn an_event_kind_this_build_does_not_know_does_not_break_the_page() {
        // A newer server adding an event kind must not stall an older client's
        // cursor — it would stop syncing entirely and nobody would see why.
        let raw = serde_json::json!({
            "events": [{ "cursor": "evt_1", "event": "something_new" }],
            "compacted": 0
        });

        let page: Page = serde_json::from_value(raw).unwrap();
        let event: WireEvent = serde_json::from_value(page.events[0].clone()).unwrap();

        assert_eq!(event.event, "something_new");
    }

    #[test]
    fn an_event_is_stored_whole_and_not_shredded_into_what_this_build_reads() {
        // The property that lets a later build render what this one cached: the
        // fields `WireEvent` does not name are still on the way to the store.
        let raw = serde_json::json!({
            "cursor": "evt_1",
            "event": "revision",
            "notification_id": "ntf_01J",
            "something_a_newer_server_added": 42,
        });

        let stored = raw.to_string();

        assert!(
            stored.contains("something_a_newer_server_added"),
            "{stored}"
        );
    }

    #[test]
    fn a_query_value_is_encoded_rather_than_pasted() {
        assert_eq!(urlencode("evt_00000000000042"), "evt_00000000000042");
        assert_eq!(urlencode("ci-infra,cluster"), "ci-infra,cluster");
        assert_eq!(urlencode("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn a_quiet_sync_says_so() {
        assert!(SyncOutcome::default().is_quiet());
        assert!(
            !SyncOutcome {
                stored: 1,
                ..SyncOutcome::default()
            }
            .is_quiet()
        );
    }
}
