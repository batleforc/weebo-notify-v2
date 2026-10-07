//! Process metrics (§13), recorded by adapters rather than use cases. Metric names
//! are a contract with external dashboards and alerts: never rename them.

use notify_domain::ChannelId;

/// How a push attempt ended; a closed set so `push_failures_total{reason}` alerts stay stable.
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

    /// A client is catching up on `events` events — `sync_backlog_size` (histogram).
    fn sync_backlog(&self, events: usize);

    /// A real-time connection opened — `realtime_connections{transport}`.
    fn stream_opened(&self, transport: StreamKind);

    /// …and closed. Called from a guard so client-dropped connections still decrement.
    fn stream_closed(&self, transport: StreamKind);

    /// The current dead-letter depth per channel — `sink_dlq_depth{channel}` (§7.1bis).
    /// Replaces the whole series so emptied queues stop alerting.
    fn dlq_depth(&self, depths: &[(ChannelId, u64)]);

    /// Render the registry in the Prometheus text format, for `GET /metrics`.
    fn render(&self) -> String;
}

/// Metrics that go nowhere: `GET /metrics` returns an empty document.
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
