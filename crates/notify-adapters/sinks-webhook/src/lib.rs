//! The webhook sink: a signed JSON POST (design §7.1bis, scenario T17).
//!
//! This is the one part of weebo-notify that is a **public API for software we will
//! never see**. A consumer writes a handler once and expects it to keep working, so
//! everything about the request is contract:
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
//! # Why the timestamp is inside the signed material
//!
//! Because that is what makes a replay detectable at all. A signature over the body
//! alone is valid for ever: anyone who captures one request can send it again next
//! month and the consumer cannot tell. Signing `"<t>.<body>"` means a consumer that
//! rejects `|now − t| > 5 min` gets replay protection out of a comparison it was
//! going to do anyway — and [`verify`] below is that check, written once so a
//! consumer can copy it rather than infer it.
//!
//! # What is retried and what is not
//!
//! Classification lives here because only the thing that spoke HTTP can do it: a
//! `503` is "later", a `410` is "never", and a connection refused is "later" too.
//! The schedule those outcomes feed into is [`notify_domain::RetryPolicy`], and the
//! two are deliberately in different crates — one is a property of the protocol, the
//! other is a promise made to operators.
//!
//! # No SSRF gate here, and where it is instead
//!
//! A webhook URL is not user input: it comes from `[[channels.managed.sinks]]`, which
//! only an operator writes (design §11). The scheme check and the refusal of
//! credentials in the URL happen where that file is read, at startup — the same
//! place that already refuses an unknown sink `type`. Repeating them per delivery
//! would suggest the value can change between two, which it cannot.

use core::fmt::Write as _;
use std::time::Duration;

use hmac::{Hmac, Mac as _};
use notify_ports::{PortError, PortResult, SinkDelivery, SinkDispatcher, SinkOutcome, SinkTarget};
use sha2::Sha256;
use time::OffsetDateTime;

/// How long a consumer has to answer.
///
/// Short enough that a hung endpoint does not hold a drain slot for a minute, long
/// enough for a handler that writes to its own database before answering. A timeout
/// is a retry, so the cost of being wrong here is one more attempt in thirty seconds.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The window a consumer should accept a signature in (design §7.1bis).
///
/// Published here because it is half of the contract: we promise the timestamp is
/// honest, they promise to refuse anything older. [`verify`] applies it.
pub const REPLAY_WINDOW: Duration = Duration::from_secs(5 * 60);

/// A `SinkDispatcher` that POSTs signed JSON.
#[derive(Debug, Clone)]
pub struct WebhookSink {
    http: reqwest::Client,
}

impl WebhookSink {
    /// Build one.
    ///
    /// # Errors
    ///
    /// If the HTTP client cannot be constructed at all, which on this platform means
    /// the TLS backend failed to initialise — an infrastructure failure, reported as
    /// one rather than deferred to the first delivery.
    pub fn new() -> PortResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            // A webhook consumer that answers with a redirect is a consumer whose
            // configuration is wrong; following it would sign a request for one host
            // and deliver it to another.
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
            // The drain resolves a target by the sink's own kind, so this cannot
            // happen without a wiring mistake — and a wiring mistake must not be
            // retried for twenty-four hours.
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
                // Everything reqwest reports here — connect, timeout, TLS, DNS — is
                // a condition that can end on its own, so it is worth another try.
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

    // The two 4xx that mean "not now" rather than "not ever". Everything else in the
    // 4xx range says the request itself is unacceptable, and repeating it for a day
    // only delays the moment somebody reads the dead letter.
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

/// The signature of `"<timestamp>.<body>"`, hex-encoded.
///
/// Exposed rather than private because it is half of a published contract: a
/// consumer implements the same three lines, and a test that reimplemented them
/// would be checking its own copy instead of ours.
#[must_use]
pub fn sign(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    // HMAC accepts a key of any length, so the fallback is unreachable — it is
    // written as an infallible zero key rather than an `expect`, because panicking
    // is denied in this workspace and would be absurd here: the worst a wrong key
    // could do is produce a signature a consumer refuses.
    let mut mac = <Hmac<Sha256> as hmac::Mac>::new_from_slice(secret).unwrap_or_else(|_| {
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
    /// The timestamp is outside [`REPLAY_WINDOW`] — a replay, or a clock that needs
    /// looking at.
    Stale,
    /// The digest does not match the body under this secret.
    Mismatch,
}

/// The reference consumer's check (design §7.1bis, scenario T17).
///
/// Written here, tested here, and quoted in the docs site: a contract a consumer has
/// to guess at is a contract that gets implemented three subtly different ways. The
/// comparison is constant-time, because the alternative leaks the digest one byte of
/// timing at a time to anyone allowed to POST at the endpoint.
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

    let age = (now.unix_timestamp() - timestamp).abs();
    if age > REPLAY_WINDOW.as_secs().cast_signed() {
        return Err(SignatureError::Stale);
    }

    let expected = sign(secret, timestamp, body);
    // Constant time over the rendered hex: both are the same length, and comparing
    // them with `==` would return early on the first differing byte.
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

/// Pull `t` and `v1` out of the header, in either order and ignoring anything else —
/// a future `v2` must not break a `v1` consumer, which is the whole reason the
/// scheme is versioned.
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
        // A future `v2=` alongside `v1=`, and the fields the other way round: both
        // are things a signing scheme is versioned in order to survive.
        let header = format!("v2=whatever,v1={},t={at}", sign(SECRET, at, BODY));

        assert_eq!(verify(SECRET, &header, BODY, now), Ok(()));
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
