//! What every HTTP push transport shares: the retry loop and its policy, and the
//! resolver that refuses to dial a non-public address.
//!
//! Its own crate rather than part of `notify-ports`, where the policy used to live:
//! the loop sends a `reqwest` request, and the port crate is the vocabulary the
//! domain is driven through — an HTTP client there would be linked by every crate
//! above it, use cases included. The policy moved here with the loop because the
//! push transports are its only callers, so the whole retry story reads in one
//! place.

pub mod retry;

use std::time::Duration;

/// Send, and again on a transient failure — throttling, a `5xx` or a refused
/// connection — under the policy of [`retry`], all of it within
/// [`retry::MAX_TOTAL`]. `None` is "unreachable": an error, or a relay that did not
/// answer in time.
///
/// Answers with the last response, whatever its status: what a status *means* is
/// each transport's business (APNs re-mints its provider token on a `403`, for
/// one), only whether it is worth asking again is shared.
pub async fn send_retrying(request: reqwest::RequestBuilder) -> Option<reqwest::Response> {
    tokio::time::timeout(retry::MAX_TOTAL, retrying(request))
        .await
        .ok()?
        .ok()
}

async fn retrying(request: reqwest::RequestBuilder) -> reqwest::Result<reqwest::Response> {
    let mut waited = Duration::ZERO;
    let mut attempt = 0;
    loop {
        // A body held in memory always clones; were it ever a stream, this sends
        // once and does not retry rather than failing.
        let Some(this) = request.try_clone() else {
            return request.send().await;
        };
        let result = this.send().await;
        let hint = match &result {
            Ok(response) if retry::is_transient(response.status().as_u16()) => retry::retry_after(
                response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok()),
            ),
            Err(error) if retry::is_retryable_error(error.is_connect(), error.is_timeout()) => None,
            _ => return result,
        };
        let Some(wait) = retry::delay(attempt, hint, waited) else {
            return result;
        };
        tokio::time::sleep(wait).await;
        waited = waited.saturating_add(wait);
        attempt = attempt.saturating_add(1);
    }
}

/// A resolver that only ever hands back public addresses.
///
/// For the transports whose endpoint a user picked (Web Push, UnifiedPush): a name
/// resolving inside the network is the same request forgery as an IP literal
/// (design §6.2), and only the resolver sees what the name became.
#[derive(Debug, Clone, Copy)]
pub struct PublicOnly;

/// What [`PublicOnly`] fails with, typed so a caller can tell a refused address
/// from a DNS failure by walking the error's sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotPublic;

impl std::fmt::Display for NotPublic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the name resolves to no public address")
    }
}

impl std::error::Error for NotPublic {}

/// Whether `error`, or any error it wraps, is [`NotPublic`].
#[must_use]
pub fn is_not_public(error: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(error), |error| error.source())
        .any(<dyn std::error::Error>::is::<NotPublic>)
}

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addrs: Vec<_> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|addr| notify_domain::is_public_address(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(Box::new(NotPublic) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// A relay on loopback answering each connection with the next of `statuses`,
    /// then closing it. Hands back its URL and how many requests it saw.
    async fn relay(
        statuses: &'static [&'static str],
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = seen.clone();
        tokio::spawn(async move {
            for status in statuses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0_u8; 4096];
                let _ = socket.read(&mut buffer).await.unwrap();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.1 {status}\r\nretry-after: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (url, seen)
    }

    #[tokio::test]
    async fn a_transient_status_is_retried_until_the_relay_recovers() {
        let (url, seen) = relay(&[
            "503 Service Unavailable",
            "429 Too Many Requests",
            "201 Created",
        ])
        .await;

        let response = send_retrying(reqwest::Client::new().post(&url).body("x"))
            .await
            .unwrap();

        assert_eq!(response.status().as_u16(), 201);
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retries_stop_at_the_policy_ceiling_and_hand_back_the_last_answer() {
        let (url, seen) = relay(&["503 Service Unavailable"; 4]).await;

        let response = send_retrying(reqwest::Client::new().post(&url).body("x"))
            .await
            .unwrap();

        assert_eq!(response.status().as_u16(), 503);
        assert_eq!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            1 + retry::MAX_RETRIES as usize
        );
    }

    #[tokio::test]
    async fn a_permanent_status_is_answered_at_once() {
        let (url, seen) = relay(&["410 Gone", "201 Created"]).await;

        let response = send_retrying(reqwest::Client::new().post(&url).body("x"))
            .await
            .unwrap();

        assert_eq!(response.status().as_u16(), 410);
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_name_resolving_inside_the_network_is_never_dialled() {
        use reqwest::dns::Resolve as _;

        let resolved = PublicOnly.resolve("localhost".parse().unwrap()).await;
        assert!(
            resolved.is_err_and(|error| is_not_public(error.as_ref())),
            "localhost is not a push service"
        );
    }
}
