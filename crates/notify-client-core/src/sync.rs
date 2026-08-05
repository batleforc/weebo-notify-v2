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
//! # Events are read through the generated client, and stored before they are
//!
//! [`Event`] comes from the contract as ADR-0009 asks (it did not, until D1 §14.4
//! gave it a schema a generator can model). What does *not* go through the generated
//! call is the page envelope: the body is taken as JSON so each event can be handed
//! to the store **verbatim**, and only then read.
//!
//! That order is the point. A progenitor struct drops the fields this build does not
//! know about, so re-serialising one would cache a *newer server's* event minus
//! everything new about it — and the cache is what a front end that lost the election
//! renders from (§9.3bis). Storing the bytes and parsing a copy costs one envelope
//! struct and keeps ADR-0013's promise that the shape on the wire is the contract.

use notify_client_api::types::Event;
use serde::Deserialize;

use crate::error::ClientError;
use crate::session::Session;
use crate::store::{CacheEntry, Store};

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
/// `events` stays a `Value` on the way in so the whole event reaches the store
/// **verbatim**, and [`Event`] reads a copy. Shredding it into the fields this build
/// happens to know would leave a later build unable to render what an earlier one
/// cached.
#[derive(Debug, Deserialize)]
struct Page {
    events: Vec<serde_json::Value>,
    /// The notifications this page says arrived late on purpose (§7.4).
    #[serde(default)]
    deferred_by_dnd: Vec<String>,
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
                // Read off the document rather than off the parsed event, so an event
                // kind this build has never heard of still moves the cursor. Refusing
                // to advance past one would stall an older client against a newer
                // server for good, which is a worse failure than not caching a row.
                //
                // Advanced per event and written per page: the cursor must never
                // claim more than the store actually holds (see `Store::set_cursor`).
                if let Some(position) = raw.get("cursor").and_then(serde_json::Value::as_str) {
                    cursor = Some(position.to_owned());
                }

                match serde_json::from_value::<Event>(raw.clone()) {
                    Ok(Event::Revision {
                        notification_id,
                        channel_id,
                        created_at,
                        revised_at,
                        ..
                    }) => {
                        store.upsert(CacheEntry {
                            id: &notification_id,
                            channel: &channel_id,
                            revised_at: &rfc3339(revised_at),
                            created_at: &rfc3339(created_at),
                            // The arrival time is stamped by the store, not passed
                            // in: it is a fact about this machine's cache, and §7.2's
                            // badge is the gap between it and `created_at`.
                            payload: &raw.to_string(),
                            // Whether that gap was asked for, which only the server
                            // knows: it holds the preference (§7.4).
                            deferred_by_dnd: page
                                .deferred_by_dnd
                                .iter()
                                .any(|id| id.as_str() == notification_id.as_str()),
                        })?;
                        outcome.stored += 1;
                    }
                    Ok(Event::Tombstone {
                        notification_id, ..
                    }) => {
                        store.remove(&notification_id)?;
                        outcome.removed += 1;
                    }
                    // Nothing for the cache to hold, for one of two reasons that
                    // happen to want the same thing. A receipt, an interaction or a
                    // preference changes a notification's detail rather than the
                    // cache's contents — the TUI reads those live, a cold catch-up
                    // has nothing to do with them. An event this build cannot read
                    // at all is a newer kind, or one whose shape moved; either way
                    // the cursor has already advanced past it.
                    Ok(_) | Err(_) => {}
                }
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

/// Back to the text the server sent.
///
/// The store keeps its timestamps as strings and orders on them, so the spelling has
/// to stay the one the wire uses: `Z` rather than `+00:00`, and no invented decimals.
fn rfc3339(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
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
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    #[test]
    fn a_sync_page_deserialises_from_the_shape_8_1_documents() {
        // The page envelope this module still reads itself, and one whole event of
        // the shape §8.1 documents.
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

        // Read through the generated type, which is what D1 bought: before it, this
        // deserialisation could not compile against an uninhabited enum.
        let event: Event = serde_json::from_value(page.events[0].clone()).unwrap();
        let Event::Revision {
            channel_id,
            created_at,
            payload,
            ..
        } = event
        else {
            panic!("the wire shape of §8.1 is a revision event");
        };

        assert_eq!(channel_id.as_str(), "ci-infra");
        assert_eq!(rfc3339(created_at), "2026-08-01T10:00:00Z");
        assert_eq!(payload.title, "etcd degraded");
    }

    #[test]
    fn an_event_kind_this_build_does_not_know_does_not_break_the_page() {
        // A newer server adding an event kind must not stall an older client's
        // cursor — it would stop syncing entirely and nobody would see why. The page
        // still parses, the event does not, and the cursor is read off the document.
        let raw = serde_json::json!({
            "events": [{ "cursor": "evt_1", "event": "something_new" }],
            "compacted": 0
        });

        let page: Page = serde_json::from_value(raw).unwrap();
        let unreadable = &page.events[0];

        assert!(serde_json::from_value::<Event>(unreadable.clone()).is_err());
        assert_eq!(
            unreadable.get("cursor").and_then(serde_json::Value::as_str),
            Some("evt_1"),
            "the cursor has to be reachable without understanding the event"
        );
    }

    #[test]
    fn an_event_is_stored_whole_and_not_shredded_into_what_this_build_reads() {
        // The property that lets a later build render what this one cached: the
        // fields the generated type does not name are still on the way to the store.
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
