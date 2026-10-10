//! The retry policy every push transport shares.
//!
//! Pure arithmetic and nothing that sends: [`crate::send_retrying`] owns the loop
//! and the sleeping, this only answers "again, and after how long?". Kept apart
//! from the loop so the policy is testable without a relay.
//!
//! Deliberately short. A push sits on the fan-out's path, and a relay that is down
//! for longer than a few seconds is better reported (the metric, then the inbox on
//! next open) than waited on.

use std::time::Duration;

/// At most this many retries after the first attempt.
pub const MAX_RETRIES: u32 = 2;

/// The most one delivery may spend sleeping between attempts, `Retry-After`
/// included. A relay asking for longer is given up on rather than overridden:
/// retrying sooner than it asked is how a rate limit becomes a ban.
pub const MAX_TOTAL_WAIT: Duration = Duration::from_secs(5);

/// The most one delivery may take in all — every attempt and every wait between
/// them. Each request has its own timeout, but a relay answering `503` just
/// before it, twice, would otherwise hold a fan-out slot for half a minute; this
/// is what makes a tarpit cost a bounded slice of the drain.
pub const MAX_TOTAL: Duration = Duration::from_secs(12);

/// The first backoff step; each retry doubles it, plus up to half again as jitter.
const BASE: Duration = Duration::from_millis(200);

/// Whether a status is worth another attempt: throttling and server-side trouble.
/// Every other `4xx` will answer the same way next time.
///
/// A transport error is retried only when the connection was refused or reset
/// before any time was spent waiting on it ([`is_retryable_error`]).
#[must_use]
pub const fn is_transient(status: u16) -> bool {
    status == 429 || status >= 500
}

/// Whether a failed request is worth another attempt, given what the HTTP client
/// reports about it: a connection that could not be made, yes — that is cheap to
/// find out and often gone a moment later. Anything that *timed out*, no: the relay
/// has already had its whole request timeout, and asking again is how a tarpit
/// turns one slow endpoint into a stalled fan-out.
#[must_use]
pub const fn is_retryable_error(connect: bool, timeout: bool) -> bool {
    connect && !timeout
}

/// Read a `Retry-After` in its delta-seconds form.
///
/// The HTTP-date form is not read and falls back to the backoff: no push relay is
/// known to send it, and parsing dates to honour a hint is not worth a dependency.
#[must_use]
pub fn retry_after(header: Option<&str>) -> Option<Duration> {
    header?.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// How long to wait before retry number `attempt` (0-based), or `None` to stop.
///
/// `waited` is what this delivery has already slept.
#[must_use]
pub fn delay(attempt: u32, retry_after: Option<Duration>, waited: Duration) -> Option<Duration> {
    if attempt >= MAX_RETRIES {
        return None;
    }
    let wait = retry_after.unwrap_or_else(|| {
        let step = BASE.saturating_mul(1 << attempt.min(8));
        step + jitter(step / 2)
    });
    (waited.saturating_add(wait) <= MAX_TOTAL_WAIT).then_some(wait)
}

/// Up to `span`, from the clock's sub-second noise.
///
/// Not random in any sense that matters for security — it only has to stop every
/// replica retrying a recovering relay on the same millisecond.
fn jitter(span: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| now.subsec_nanos());
    let span = u64::try_from(span.as_millis()).unwrap_or(u64::MAX).max(1);
    Duration::from_millis(u64::from(nanos) % span)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_throttling_and_server_errors_are_retried() {
        assert!(is_transient(429));
        assert!(is_transient(500));
        assert!(is_transient(503));
        assert!(!is_transient(200));
        assert!(!is_transient(400));
        assert!(!is_transient(404));
        assert!(!is_transient(410));
        assert!(!is_transient(413));
    }

    #[test]
    fn a_timeout_is_never_retried_a_refused_connection_is() {
        assert!(is_retryable_error(true, false));
        assert!(
            !is_retryable_error(true, true),
            "a connect timeout is a timeout"
        );
        assert!(!is_retryable_error(false, true));
        assert!(!is_retryable_error(false, false));
    }

    #[test]
    fn retry_after_reads_seconds_and_ignores_dates() {
        assert_eq!(retry_after(Some(" 3 ")), Some(Duration::from_secs(3)));
        assert_eq!(retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT")), None);
        assert_eq!(retry_after(None), None);
    }

    #[test]
    fn backoff_grows_with_jitter_and_stops_after_the_budget() {
        let first = delay(0, None, Duration::ZERO).unwrap_or_default();
        assert!(first >= BASE && first < BASE * 3 / 2, "{first:?}");
        let second = delay(1, None, first).unwrap_or_default();
        assert!(second >= BASE * 2 && second < BASE * 3, "{second:?}");
        assert_eq!(delay(MAX_RETRIES, None, Duration::ZERO), None);
    }

    #[test]
    fn retry_after_is_honoured_or_given_up_on_never_shortened() {
        let asked = Duration::from_secs(2);
        assert_eq!(delay(0, Some(asked), Duration::ZERO), Some(asked));
        assert_eq!(delay(0, Some(MAX_TOTAL_WAIT + asked), Duration::ZERO), None);
        assert_eq!(delay(1, Some(asked), Duration::from_secs(4)), None);
    }
}
