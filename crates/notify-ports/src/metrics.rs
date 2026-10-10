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

/// How far the retention jobs are behind, as the leader last measured it
/// (design §7.2bis, §13).
///
/// A purge that cannot keep up does not fail — it just deletes less than arrives,
/// every pass, until the disk says so. These are what make that visible first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionBacklog {
    /// Age in seconds of the oldest event row, retained or not. Compared against
    /// the event retention it says how far the prune is behind.
    pub oldest_event_age_seconds: u64,
    /// Event rows already past retention and still there.
    pub events_prunable: u64,
    /// Notifications whose TTL ran out and that have not been expired yet.
    pub pending_expiry: u64,
    /// Tombstoned rows past their grace period and not purged yet.
    pub pending_purge: u64,
}

/// How the fan-out outbox is doing (ADR-0025), as the leader last measured it.
///
/// A drain that cannot keep up, or rows piling up dead, does not fail anything
/// anybody sees — phones just stop buzzing. These are what an alert watches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FanoutBacklog {
    /// Rows still owed.
    pub pending: u64,
    /// Rows retired as dead, waiting for an operator.
    pub dead: u64,
    /// Age in seconds of the oldest row still owed; zero when there is none.
    pub oldest_pending_age_seconds: u64,
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

    /// A background job finished a pass — `background_job_last_success_timestamp_seconds{task}`
    /// when `ok`, `background_job_failures_total{task}` when not (a panic included).
    ///
    /// The pair an operator alerts on: a job whose loop stopped looks exactly like a
    /// job with nothing to do, unless something says when it last *worked*. Defaulted
    /// so a double that does not care need not say so.
    fn job_finished(&self, _job: &'static str, _ok: bool) {}

    /// A reconcile pass refused its destructive half because it would have removed
    /// more than `[reconcile] max_removals` — `reconcile_removals_refused_total`.
    fn reconcile_removals_refused(&self) {}

    /// The retention backlog — `retention_backlog_rows{kind}` and
    /// `retention_oldest_event_age_seconds`.
    ///
    /// Default no-op so a double that does not care need not say so.
    fn retention_backlog(&self, _backlog: &RetentionBacklog) {}

    /// The fan-out outbox — `fanout_outbox_rows{kind}` and
    /// `fanout_outbox_oldest_age_seconds`. Default no-op, like the one above.
    fn fanout_backlog(&self, _backlog: &FanoutBacklog) {}

    /// How long the oldest committed event has been held back behind the event
    /// watermark — `event_watermark_lag_seconds` (ADR-0028). Zero when nothing is.
    ///
    /// The watermark waits for the oldest transaction still running, so a
    /// long-running write on the primary delays delivery of everything committed
    /// after it began; this is what makes that visible. Default no-op.
    fn event_watermark_lag(&self, _seconds: u64) {}

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
        assert_eq!(metrics.render(), "");
    }
}
