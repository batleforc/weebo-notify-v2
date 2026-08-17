//! What this process is willing to be measured on (design §13).
//!
//! A port rather than a global recorder, for the reason every other port here is
//! one: the composition root decides whether the numbers go to a Prometheus
//! registry or nowhere, and nothing above it can tell. A test that wants to assert
//! "the publish was counted once" gets a double instead of a scrape.
//!
//! # Why this is recorded by the adapters and not by the use cases
//!
//! Everything below is a fact about **the process**, not about the domain: how many
//! connections this replica holds, how long a relay took to answer, how deep the
//! dead-letter queue is. A use case that recorded its own latency would be a use
//! case that knows it is being watched — and, worse, one whose in-memory test suite
//! carries a metrics handle for no reason.
//!
//! The one that looks like a domain fact, `notifications_published_total`, is
//! recorded where the request is accepted (design §8.3 — all ingestion converges on
//! two endpoints, so counting them counts every publish, from `curl` to the CLI to a
//! CI job).
//!
//! # Names are the contract
//!
//! These strings end up in dashboards and alert rules that live outside this
//! repository. Renaming one is a breaking change for whoever wrote the alert, so
//! the names come from §13 and are repeated here rather than invented by an adapter.

use notify_domain::ChannelId;

/// How a push attempt ended, as the failure counter labels it.
///
/// A closed set: `push_failures_total{reason}` is what somebody alerts on, and a
/// free-form reason would make that alert match nothing the day a transport words
/// its error differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    /// The relay accepted it.
    Delivered,
    /// The relay says this subscription is gone; the device was revoked.
    Expired,
    /// The relay refused, and it may work later.
    Failed,
    /// The transport itself was unusable — not configured, or unreachable.
    Unusable,
}

impl PushResult {
    /// The `reason` label. `None` for a delivery, which is not a failure.
    #[must_use]
    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::Delivered => None,
            Self::Expired => Some("expired"),
            Self::Failed => Some("failed"),
            Self::Unusable => Some("unusable"),
        }
    }
}

/// Which real-time transport a connection is held on (design §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// Server-sent events, the reference transport.
    Sse,
    /// The WebSocket alternative.
    WebSocket,
}

impl StreamKind {
    /// The `transport` label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sse => "sse",
            Self::WebSocket => "ws",
        }
    }
}

/// The numbers design §13 publishes.
pub trait Metrics: Send + Sync {
    /// A notification revision was accepted — `notifications_published_total{channel}`.
    fn published(&self, channel: &ChannelId);

    /// One push attempt finished — `push_delivery_duration_seconds{transport}` and,
    /// when it did not arrive, `push_failures_total{transport,reason}`.
    fn push_finished(&self, transport: &'static str, result: PushResult, seconds: f64);

    /// A client is catching up on `events` events — `sync_backlog_size`.
    ///
    /// A histogram rather than a gauge: the question an operator has is "how far
    /// behind are the clients that reconnect", and a gauge would answer with
    /// whichever one asked last.
    fn sync_backlog(&self, events: usize);

    /// A real-time connection opened — `realtime_connections{transport}`.
    fn stream_opened(&self, transport: StreamKind);

    /// …and closed. Called from a guard rather than at the end of the handler, so a
    /// connection dropped by the client still decrements.
    fn stream_closed(&self, transport: StreamKind);

    /// The current dead-letter depth per channel — `sink_dlq_depth{channel}`
    /// (design §7.1bis).
    ///
    /// Replaces the whole series rather than adding to it: a channel that no longer
    /// appears has nothing stuck, and leaving its last value behind would keep an
    /// alert firing for a queue somebody has already emptied.
    fn dlq_depth(&self, depths: &[(ChannelId, u64)]);

    /// Render the registry in the Prometheus text format, for `GET /metrics`.
    ///
    /// On the port because the endpoint that serves it has no business knowing which
    /// registry it came from — and because the no-op implementation answering with
    /// nothing is exactly what a deployment without metrics should serve.
    fn render(&self) -> String;
}

/// Metrics that go nowhere.
///
/// The default in tests, and the honest answer for any build that has not wired an
/// exporter: `GET /metrics` then returns an empty document rather than an error,
/// because "this instance measures nothing" is a configuration answer and not a
/// failure.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMetrics;

impl Metrics for NoMetrics {
    fn published(&self, _channel: &ChannelId) {}
    fn push_finished(&self, _transport: &'static str, _result: PushResult, _seconds: f64) {}
    fn sync_backlog(&self, _events: usize) {}
    fn stream_opened(&self, _transport: StreamKind) {}
    fn stream_closed(&self, _transport: StreamKind) {}
    fn dlq_depth(&self, _depths: &[(ChannelId, u64)]) {}

    fn render(&self) -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delivery_is_not_a_failure_and_every_other_outcome_names_itself() {
        assert_eq!(PushResult::Delivered.reason(), None);

        // The label set an alert rule is written against, so it is closed and
        // spelled here rather than by whichever transport failed.
        for (result, reason) in [
            (PushResult::Expired, "expired"),
            (PushResult::Failed, "failed"),
            (PushResult::Unusable, "unusable"),
        ] {
            assert_eq!(result.reason(), Some(reason));
        }
    }

    #[test]
    fn an_instance_that_measures_nothing_serves_an_empty_document() {
        let metrics = NoMetrics;
        metrics.published(&ChannelId::parse("ci-infra").unwrap_or_else(|_| unreachable!()));
        assert!(metrics.render().is_empty());
    }
}
