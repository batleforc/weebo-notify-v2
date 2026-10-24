//! The webhook sink (design §7.1bis, T17): a signed JSON POST; the request shape is a public contract.
//!
//! ```text
//! POST <configured url>
//! Content-Type:      application/json
//! X-Weebo-Event:     revision
//! X-Weebo-Delivery:  dlv_01J…                     ← idempotency key, stable across retries
//! X-Weebo-Signature: t=1754400000,v1=9f86d0…      ← hex(hmac_sha256(secret, "<t>.<body>"))
//!
//! { … the stream event, unchanged (design §8.1) … }
//! ```
//!
//! The timestamp is signed so consumers can reject replays ([`verify`]). URLs are operator-written
//! config, validated at startup, so there is no per-delivery SSRF gate.

use core::fmt::Write as _;
use std::time::Duration;

use hmac::{Hmac, Mac as _};
use notify_ports::{PortError, PortResult, SinkDelivery, SinkDispatcher, SinkOutcome, SinkTarget};
use sha2::Sha256;
use time::OffsetDateTime;

/// How long a consumer has to answer; a timeout is just a retry.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The window a consumer should accept a signature in (design §7.1bis); [`verify`] applies it.
pub const REPLAY_WINDOW: Duration = Duration::from_mins(5);

/// A `SinkDispatcher` that POSTs signed JSON.
#[derive(Debug, Clone)]
pub struct WebhookSink {
    http: reqwest::Client,
}

impl WebhookSink {
    /// If the HTTP client (TLS backend) cannot be constructed.
    pub fn new() -> PortResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            // Following a redirect would deliver a request signed for one host to another.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("weebo-notify/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| PortError::unavailable("sinks.webhook", error))?;

        Ok(Self { http })
    }
}

#[async_trait::async_trait]
impl SinkDispatcher for WebhookSink {
    fn handles(&self) -> &'static [&'static str] {
        &["webhook"]
    }

    async fn deliver(
        &self,
        delivery: &SinkDelivery,
        target: &SinkTarget,
    ) -> PortResult<SinkOutcome> {
        let SinkTarget::Webhook { url, secret } = target else {
            // Only a wiring mistake reaches here, and it must not be retried for a day.
            return Ok(SinkOutcome::Rejected {
                reason: "webhook dispatcher handed a target of another kind".to_owned(),
            });
        };

        let body = serde_json::to_vec(&delivery.event)
            .map_err(|error| PortError::corrupt("sink.event", error))?;
        let timestamp = OffsetDateTime::now_utc().unix_timestamp();
        let signature = sign(secret.as_bytes(), timestamp, &body);

        let response = self
            .http
            .post(url.clone())
            .header("content-type", "application/json")
            .header("x-weebo-event", delivery.event.kind().as_str())
            .header("x-weebo-delivery", delivery.delivery_id.to_string())
            .header("x-weebo-signature", format!("t={timestamp},v1={signature}"))
            .body(body)
            .send()
            .await;

        Ok(match response {
            Ok(response) => classify(response.status()),
            Err(error) => {
                // Connect, timeout, TLS, DNS: all can recover, so retry. `without_url`: the path
                // may hold the consumer's credential, and this text lands in `last_error`.
                let error = error.without_url();
                tracing::debug!(?error, delivery = %delivery.delivery_id, "webhook attempt failed");
                SinkOutcome::Retry {
                    reason: error.to_string(),
                }
            }
        })
    }
}

/// Turn a status into a verdict.
fn classify(status: reqwest::StatusCode) -> SinkOutcome {
    if status.is_success() {
        return SinkOutcome::Delivered;
    }

    // The only 4xx meaning "not now"; other 4xx are permanent.
    let retryable = status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS;

    let reason = format!("HTTP {}", status.as_u16());
    if retryable {
        SinkOutcome::Retry { reason }
    } else {
        SinkOutcome::Rejected { reason }
    }
}

/// The signature of `"<timestamp>.<body>"`, hex-encoded; public because consumers implement the same.
#[must_use]
pub fn sign(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    // HMAC accepts any key length, so the fallback is unreachable (panics are denied here).
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(secret).unwrap_or_else(|_| {
        <Hmac<Sha256> as hmac::digest::KeyInit>::new(&hmac::digest::Key::<Hmac<Sha256>>::default())
    });

    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);

    let digest = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Why a signature was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    /// The header was not `t=<unix>,v1=<hex>`.
    Malformed,
    /// The timestamp is outside [`REPLAY_WINDOW`]: a replay or a skewed clock.
    Stale,
    /// The digest does not match the body under this secret.
    Mismatch,
}

/// The reference consumer's check (design §7.1bis, T17), quoted in the docs; constant-time.
///
/// # Errors
///
/// See [`SignatureError`].
pub fn verify(
    secret: &[u8],
    header: &str,
    body: &[u8],
    now: OffsetDateTime,
) -> Result<(), SignatureError> {
    let (timestamp, digest) = parse(header).ok_or(SignatureError::Malformed)?;

    // `abs_diff`, not `(a - b).abs()`: `t` is attacker-supplied, and `t = i64::MIN`
    // would overflow the subtraction — a panic in debug, a wrapped "fresh" age in
    // release.
    let age = now.unix_timestamp().abs_diff(timestamp);
    if age > REPLAY_WINDOW.as_secs() {
        return Err(SignatureError::Stale);
    }

    let expected = sign(secret, timestamp, body);
    // Constant time: `==` would leak the first differing byte.
    let matched = expected.len() == digest.len()
        && expected
            .bytes()
            .zip(digest.bytes())
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
            == 0;

    if matched {
        Ok(())
    } else {
        Err(SignatureError::Mismatch)
    }
}

/// Pull `t` and `v1` from the header in any order, ignoring the rest (so a future `v2` is harmless).
fn parse(header: &str) -> Option<(i64, &str)> {
    let mut timestamp = None;
    let mut digest = None;

    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", value)) => timestamp = value.parse::<i64>().ok(),
            Some(("v1", value)) => digest = Some(value),
            _ => {}
        }
    }

    Some((timestamp?, digest?))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    const SECRET: &[u8] = b"shh";
    const BODY: &[u8] = br#"{"event":"revision"}"#;

    fn header(at: i64) -> String {
        format!("t={at},v1={}", sign(SECRET, at, BODY))
    }

    #[test]
    fn critical_t17_what_we_sign_is_what_the_reference_consumer_accepts() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            verify(SECRET, &header(now.unix_timestamp()), BODY, now),
            Ok(())
        );
    }

    #[test]
    fn critical_t17_a_replay_from_six_minutes_ago_is_refused() {
        let now = OffsetDateTime::now_utc();
        let captured = header(now.unix_timestamp() - 6 * 60);

        assert_eq!(
            verify(SECRET, &captured, BODY, now),
            Err(SignatureError::Stale),
            "the timestamp is inside the signed material precisely so this is catchable"
        );
    }

    #[test]
    fn a_body_changed_after_signing_does_not_verify() {
        let now = OffsetDateTime::now_utc();
        let header = header(now.unix_timestamp());

        assert_eq!(
            verify(SECRET, &header, br#"{"event":"tombstone"}"#, now),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn another_secret_does_not_verify() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            verify(b"guess", &header(now.unix_timestamp()), BODY, now),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn the_header_is_parsed_by_name_and_not_by_position() {
        let now = OffsetDateTime::now_utc();
        let at = now.unix_timestamp();
        // A future `v2=` and reversed order must both still verify.
        let header = format!("v2=whatever,v1={},t={at}", sign(SECRET, at, BODY));

        assert_eq!(verify(SECRET, &header, BODY, now), Ok(()));
    }

    #[test]
    fn an_extreme_timestamp_is_stale_rather_than_an_overflow() {
        let now = OffsetDateTime::now_utc();
        for at in [i64::MIN, i64::MAX] {
            assert_eq!(
                verify(SECRET, &format!("t={at},v1=00"), BODY, now),
                Err(SignatureError::Stale)
            );
        }
    }

    #[test]
    fn a_header_that_is_not_a_signature_is_malformed_rather_than_a_mismatch() {
        let now = OffsetDateTime::now_utc();
        for header in ["", "t=notanumber,v1=ab", "v1=ab", "t=1"] {
            assert_eq!(
                verify(SECRET, header, BODY, now),
                Err(SignatureError::Malformed),
                "`{header}` is not a signature"
            );
        }
    }

    #[test]
    fn only_the_statuses_that_can_change_are_retried() {
        use reqwest::StatusCode;

        assert_eq!(classify(StatusCode::OK), SinkOutcome::Delivered);
        assert_eq!(classify(StatusCode::NO_CONTENT), SinkOutcome::Delivered);

        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            assert!(
                matches!(classify(status), SinkOutcome::Retry { .. }),
                "{status} can end on its own"
            );
        }

        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::NOT_FOUND,
            StatusCode::GONE,
            StatusCode::MOVED_PERMANENTLY,
        ] {
            assert!(
                matches!(classify(status), SinkOutcome::Rejected { .. }),
                "{status} will say the same thing tomorrow"
            );
        }
    }
}
