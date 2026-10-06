//! Rate limits counted in the database (design §8.2, T14), so the limit holds across replicas.
//! A fixed window: the §8.2 burst allowance covers what a sliding window would smooth.

use notify_ports::{Allowance, PortResult, Quota, RateLimiter, StreamLeases};
use time::{Duration, OffsetDateTime};

use crate::PgStore;
use crate::mapping::fail;

/// Every window is a minute, which is the unit design §8.2 states its limits in.
const WINDOW: Duration = Duration::minutes(1);

/// The published default for each quota (design §8.2); a closed match so new variants must decide.
const fn default_ceiling(quota: &Quota) -> i32 {
    match quota {
        // 60/min with a burst of 120: the burst is the ceiling.
        Quota::PublishPerKey { .. } => 120,
        // Protects the fan-out rather than the publisher.
        Quota::PublishPerChannel { .. } => 600,
        Quota::TokenExchange { .. } => 10,
        // Full-text is the expensive read; the command palette debounces client-side.
        Quota::Search { .. } => 30,
    }
}

/// What this request is measured against: the configured override (in either direction), else the default.
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

        // Consumed up front in one statement; the conflict branch resets a rolled-over window.
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
            // Seconds until the window rolls, for an exact `Retry-After`.
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
    /// Drop counters whose window has long rolled over; housekeeping only.
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

/// How often a replica should call [`StreamLeases::renew`]; leases live 90 s unrenewed.
pub const LEASE_RENEW: std::time::Duration = std::time::Duration::from_secs(30);

#[async_trait::async_trait]
impl StreamLeases for PgStore {
    async fn acquire(
        &self,
        principal: &str,
        replica: &str,
        max: usize,
    ) -> PortResult<Option<String>> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| fail("stream lease", e))?;
        // Serialises one principal's concurrent connects; other principals don't wait.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(principal)
            .execute(&mut *tx)
            .await
            .map_err(|e| fail("stream lease", e))?;
        let (held,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM stream_leases WHERE principal = $1 AND expires_at > now()",
        )
        .bind(principal)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| fail("stream lease", e))?;
        if usize::try_from(held).unwrap_or(usize::MAX) >= max {
            return Ok(None);
        }
        let (id,): (String,) = sqlx::query_as(
            "INSERT INTO stream_leases (principal, replica, expires_at)
             VALUES ($1, $2, now() + interval '90 seconds') RETURNING id::text",
        )
        .bind(principal)
        .bind(replica)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| fail("stream lease", e))?;
        tx.commit().await.map_err(|e| fail("stream lease", e))?;
        Ok(Some(id))
    }

    async fn release(&self, lease: &str) -> PortResult<()> {
        sqlx::query("DELETE FROM stream_leases WHERE id = $1::uuid")
            .bind(lease)
            .execute(self.pool())
            .await
            .map_err(|e| fail("stream lease", e))?;
        Ok(())
    }

    async fn renew(&self, replica: &str) -> PortResult<()> {
        sqlx::query(
            "UPDATE stream_leases SET expires_at = now() + interval '90 seconds' WHERE replica = $1",
        )
        .bind(replica)
        .execute(self.pool())
        .await
        .map_err(|e| fail("stream lease", e))?;
        sqlx::query("DELETE FROM stream_leases WHERE expires_at < now()")
            .execute(self.pool())
            .await
            .map_err(|e| fail("stream lease", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing assertion is a failing test")]

    use super::*;

    #[test]
    fn every_quota_lands_in_its_own_row() {
        // Per subject, never global (T14), and keyed by key id, never its secret.
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
        // §8.2: the per-channel override wins in both directions.
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
        // An override past `i32` is a typo: fall back to the default.
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
