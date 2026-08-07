//! Rate limits, counted in the database (design §8.2, scenario T14).
//!
//! Shared state rather than a counter per process: with several replicas an
//! in-process counter enforces `n × 60/min` rather than `60/min`, and the limit a
//! publisher was told about would depend on which replica answered.
//!
//! A **fixed window**, not a sliding one. A sliding window costs a row per request
//! and gives a smoother curve; the burst allowance in §8.2 already covers the edge
//! this would smooth, and the failure it prevents — a CI job in a retry loop
//! degrading its own publishing and nobody else's — does not need that precision.

use notify_ports::{Allowance, PortResult, Quota, RateLimiter};
use time::{Duration, OffsetDateTime};

use crate::PgStore;
use crate::mapping::fail;

/// Every window is a minute, which is the unit design §8.2 states its limits in.
const WINDOW: Duration = Duration::minutes(1);

/// The published default for each quota, straight from design §8.2.
///
/// A closed match rather than a lookup table: adding a [`Quota`] variant then fails
/// to compile here until somebody decides what it is worth.
const fn default_ceiling(quota: &Quota) -> i32 {
    match quota {
        // 60/min with a burst of 120: the burst is the ceiling, and the steady rate
        // is what a well-behaved publisher stays under.
        Quota::PublishPerKey { .. } => 120,
        // Protects the fan-out rather than the publisher.
        Quota::PublishPerChannel { .. } => 600,
        Quota::TokenExchange { .. } => 10,
        // Full-text is the expensive read; the command palette debounces client-side.
        Quota::Search { .. } => 30,
    }
}

/// What this request is actually measured against.
///
/// The configured override of design §8.2 wins when there is one, and the default
/// applies when there is not. Nothing is clamped to the default in either
/// direction: an operator who writes `publish_per_min = 5000` on a channel meant
/// to carry a firehose has said so in GitOps (§11), and silently holding them to
/// 600 would be a limit that lies about itself.
fn ceiling(quota: &Quota, configured: Option<u32>) -> i32 {
    configured
        .and_then(|limit| i32::try_from(limit).ok())
        .unwrap_or_else(|| default_ceiling(quota))
}

/// The row a quota counts against.
fn key(quota: &Quota) -> String {
    match quota {
        Quota::PublishPerKey { key } => format!("publish.key:{key}"),
        Quota::PublishPerChannel { channel } => format!("publish.channel:{channel}"),
        Quota::TokenExchange { subject } => format!("token.exchange:{subject}"),
        Quota::Search { principal } => format!("search:{principal}"),
    }
}

#[async_trait::async_trait]
impl RateLimiter for PgStore {
    async fn check(
        &self,
        quota: &Quota,
        configured: Option<u32>,
        now: OffsetDateTime,
    ) -> PortResult<Allowance> {
        let window_start = floor_to_window(now);

        // Consumed on the way in, in one statement: a caller that is over the limit
        // is refused before doing the work rather than after. The conflict branch
        // resets the counter when the stored window has rolled over, which is what
        // makes this a fixed window without a sweeper job.
        let (count, start): (i32, OffsetDateTime) = sqlx::query_as(
            "INSERT INTO rate_limits (quota_key, window_start, count)
             VALUES ($1, $2, 1)
             ON CONFLICT (quota_key) DO UPDATE
                 SET window_start = CASE
                         WHEN rate_limits.window_start < $2 THEN $2
                         ELSE rate_limits.window_start END,
                     count = CASE
                         WHEN rate_limits.window_start < $2 THEN 1
                         ELSE rate_limits.count + 1 END
             RETURNING count, window_start",
        )
        .bind(key(quota))
        .bind(window_start)
        .fetch_one(self.pool())
        .await
        .map_err(|error| fail("rate_limit", error))?;

        let ceiling = ceiling(quota, configured);
        if count > ceiling {
            // Seconds until the window rolls, so `Retry-After` is a fact rather than
            // a guess a client has to back off around.
            let retry_after = (start + WINDOW - now).whole_seconds().max(1);

            return Ok(Allowance::Limited {
                retry_after_seconds: u32::try_from(retry_after).unwrap_or(60),
            });
        }

        Ok(Allowance::Allowed {
            remaining: u32::try_from(ceiling - count).unwrap_or(0),
        })
    }
}

impl PgStore {
    /// Drop counters whose window has long rolled over.
    ///
    /// Not required for correctness — a stale row is reset on its next use — but a
    /// key that is used once and never again would otherwise stay forever, and this
    /// table would grow with every API key that ever published.
    ///
    /// # Errors
    ///
    /// Fails when the database cannot be reached.
    pub async fn prune_rate_limits(&self, now: OffsetDateTime) -> PortResult<u64> {
        let pruned = sqlx::query("DELETE FROM rate_limits WHERE window_start < $1")
            .bind(now - WINDOW * 2)
            .execute(self.pool())
            .await
            .map_err(|error| fail("rate_limit", error))?;

        Ok(pruned.rows_affected())
    }
}

/// The start of the minute `now` falls in.
fn floor_to_window(now: OffsetDateTime) -> OffsetDateTime {
    let seconds = i64::from(now.second());
    now.replace_nanosecond(0)
        .unwrap_or(now)
        .saturating_sub(Duration::seconds(seconds))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing assertion is a failing test")]

    use super::*;

    #[test]
    fn every_quota_lands_in_its_own_row() {
        // Limits are per subject and never global: a noisy publisher must degrade
        // its own publishing and nobody else's (scenario T14).
        //
        // The reference is the key's *id*, never its secret — a quota row keyed on
        // a credential would put one in a table nobody thinks of as a secret store.
        let one = key(&Quota::PublishPerKey {
            key: "key_01jqxr8m4n0000000000000001".to_owned(),
        });
        let other = key(&Quota::PublishPerKey {
            key: "key_01jqxr8m4n0000000000000002".to_owned(),
        });
        let channel = key(&Quota::PublishPerChannel {
            channel: "ci-infra".to_owned(),
        });

        assert_ne!(one, other);
        assert_ne!(one, channel);
    }

    #[test]
    fn the_window_starts_on_the_minute() {
        let now = OffsetDateTime::from_unix_timestamp(1_785_745_962).unwrap();
        let start = floor_to_window(now);

        assert_eq!(start.second(), 0);
        assert_eq!(start.nanosecond(), 0);
        assert!(start <= now && now - start < WINDOW);
    }

    #[test]
    fn the_published_ceilings_are_the_ones_in_the_design() {
        assert_eq!(
            ceiling(&Quota::PublishPerKey { key: String::new() }, None),
            120,
            "60/min with a burst of 120 (design §8.2)"
        );
        assert_eq!(
            ceiling(
                &Quota::PublishPerChannel {
                    channel: String::new()
                },
                None
            ),
            600
        );
        assert_eq!(
            ceiling(
                &Quota::TokenExchange {
                    subject: String::new()
                },
                None
            ),
            10
        );
        assert_eq!(
            ceiling(
                &Quota::Search {
                    principal: String::new()
                },
                None
            ),
            30
        );
    }

    #[test]
    fn a_channel_that_configures_its_own_ceiling_gets_it() {
        // §8.2: "défauts globaux, surchargeables par canal". The override wins in
        // both directions — a channel can be given more room than the default as
        // well as less, because an operator who writes the number has said what
        // they want and a limit that silently ignores it is worse than no limit.
        let channel = Quota::PublishPerChannel {
            channel: "ci-infra".to_owned(),
        };

        assert_eq!(ceiling(&channel, Some(30)), 30, "a quieter channel");
        assert_eq!(ceiling(&channel, Some(5_000)), 5_000, "a firehose");
        assert_eq!(
            ceiling(&channel, None),
            600,
            "and no configuration is the published default"
        );
    }

    #[test]
    fn a_configured_ceiling_too_large_for_the_counter_falls_back() {
        // The counter is an `i32` because that is what the column is. A number past
        // it is an operator typo, and the published default is a far better answer
        // than a panic or a silently negative ceiling that refuses everything.
        assert_eq!(
            ceiling(
                &Quota::PublishPerChannel {
                    channel: String::new()
                },
                Some(u32::MAX)
            ),
            600
        );
    }
}
