//! Catching up: `GET /sync` into the local store (design §7.2, §9.3bis).
//!
//! Events are stored verbatim before being parsed: a generated struct would drop fields
//! a newer server added, and the cache is what an election loser renders from (ADR-0013).

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

/// A sync page; `events` stay raw so the store gets them verbatim.
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
        let mut first_page = true;

        loop {
            let page = self.fetch_page(cursor.as_deref(), channels, device).await?;

            for raw in &page.events {
                // Read off the raw document so an unknown event kind still advances the
                // cursor; written per page so it never claims more than the store holds.
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
                            payload: &raw.to_string(),
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
                    // Other kinds don't change the cache; unreadable ones are newer kinds.
                    Ok(_) | Err(_) => {}
                }
            }

            if let Some(last) = &cursor {
                store.set_cursor(last)?;
            }

            // Both are counted over the whole window from the cursor the page was
            // asked with, so the first page already holds the catch-up's total and
            // every later one a subset of it: summing pages would over-count.
            if first_page {
                outcome.compacted = page.compacted;
                outcome.expired = page
                    .expired
                    .into_iter()
                    .map(|entry| (entry.channel, entry.count))
                    .collect();
                first_page = false;
            }

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
        let mut url =
            url::Url::parse(&format!("{}/api/v1/sync", self.base_url())).map_err(|error| {
                ClientError::Invalid {
                    detail: error.to_string(),
                }
            })?;
        for (key, value) in [
            ("cursor", cursor),
            ("channels", channels),
            ("device", device),
        ] {
            if let Some(value) = value {
                url.query_pairs_mut().append_pair(key, value);
            }
        }

        let response = self
            .http()
            .get(url)
            .send()
            .await
            .map_err(ClientError::Transport)?;

        let status = response.status();
        if !status.is_success() {
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

/// The wire spelling (`Z`, no invented decimals): the store orders on these strings.
fn rfc3339(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
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
