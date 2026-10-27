//! The registry behind `GET /metrics` (design §13). Family names are an external contract (alert rules).
//! Only bounded labels (`channel`, `transport`, `reason`); never ids or endpoints. The published
//! counter keeps its first [`MAX_CHANNEL_SERIES`] channels and folds the rest into `channel="other"`.
//! `dlq_depth` is set by the leader only, so followers report it absent rather than zero.

use std::sync::Mutex;

use notify_domain::ChannelId;
use notify_ports::{Metrics, PushResult, StreamKind};
use prometheus_client::encoding::{EncodeLabelSet, text::encode};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

/// How many distinct `channel` values `notifications_published_total` carries
/// before the rest are counted as `other`.
///
/// First come, first kept, for the life of the process: a series that changed
/// meaning mid-scrape would make every `rate()` over it wrong. 256 is past any
/// instance whose channels a dashboard could still show one line each.
pub const MAX_CHANNEL_SERIES: usize = 256;

/// The label everything past [`MAX_CHANNEL_SERIES`] is counted under.
const OTHER_CHANNEL: &str = "other";

/// `{channel="ci-infra"}`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ChannelLabel {
    channel: String,
}

/// `{kind="events"}`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct KindLabel {
    kind: &'static str,
}

/// `{transport="webpush"}`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OutcomeLabel {
    outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct TransportLabel {
    transport: String,
}

/// `{task="sink-drain"}`.
///
/// `task` and not `job`: Prometheus attaches its own `job` label to every scraped
/// series, and a series carrying one too gets renamed to `exported_job` — which
/// every alert written against `job=` would then silently miss.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct JobLabel {
    task: String,
}

/// `{transport="webpush",reason="expired"}`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct FailureLabel {
    transport: String,
    reason: String,
}

/// Everything this process is measured on.
#[derive(Debug)]
pub struct PrometheusMetrics {
    /// Locked because `dlq_depth` clears a family and rendering needs a consistent set.
    registry: Mutex<Registry>,
    published: Family<ChannelLabel, Counter>,
    /// The channels `published` has a series for, to enforce the cap.
    published_channels: Mutex<std::collections::HashSet<String>>,
    retention_rows: Family<KindLabel, Gauge>,
    retention_event_age: Gauge,
    fanout_rows: Family<KindLabel, Gauge>,
    fanout_age: Gauge,
    watermark_lag: Gauge,
    push_duration: Family<TransportLabel, Histogram>,
    push_failures: Family<FailureLabel, Counter>,
    sync_backlog: Histogram,
    connections: Family<TransportLabel, Gauge>,
    dlq: Family<ChannelLabel, Gauge>,
    job_success: Family<JobLabel, Gauge>,
    job_failures: Family<JobLabel, Counter>,
    reconcile_refused: Counter,
    feeds: FeedMetrics,
}

#[derive(Debug)]
struct FeedMetrics {
    polls: Family<OutcomeLabel, Counter>,
    items: Family<OutcomeLabel, Counter>,
    failing: Gauge,
}

impl Default for PrometheusMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl PrometheusMetrics {
    /// Build the registry with every family this build publishes.
    #[must_use]
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let published = Family::<ChannelLabel, Counter>::default();
        registry.register(
            "notifications_published",
            "Notification revisions accepted, by channel",
            published.clone(),
        );

        // Up to a stalled relay; the default buckets stop at 10s, past the transport timeouts.
        let push_duration = Family::<TransportLabel, Histogram>::new_with_constructor(|| {
            Histogram::new([0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0])
        });
        registry.register_with_unit(
            "push_delivery_duration",
            "How long a push relay took to answer",
            prometheus_client::registry::Unit::Seconds,
            push_duration.clone(),
        );

        let push_failures = Family::<FailureLabel, Counter>::default();
        registry.register(
            "push_failures",
            "Push attempts that did not arrive, by transport and reason",
            push_failures.clone(),
        );

        // Powers of four: only the order of magnitude of a catch-up matters.
        let sync_backlog = Histogram::new([1.0, 4.0, 16.0, 64.0, 256.0, 1_024.0]);
        registry.register(
            "sync_backlog_size",
            "Events returned to a client catching up",
            sync_backlog.clone(),
        );

        let connections = Family::<TransportLabel, Gauge>::default();
        registry.register(
            "realtime_connections",
            "Stream connections this replica is holding, by transport",
            connections.clone(),
        );

        let dlq = Family::<ChannelLabel, Gauge>::default();
        registry.register(
            "sink_dlq_depth",
            "Sink deliveries given up on, by channel — reported by the leader only",
            dlq.clone(),
        );

        // Seconds since the epoch, so `time() - x > 300` is the whole alert: a loop
        // that died stops moving this, which a failure counter alone cannot show.
        let job_success = Family::<JobLabel, Gauge>::default();
        registry.register(
            "background_job_last_success_timestamp_seconds",
            "When each background job last completed a pass without error",
            job_success.clone(),
        );
        let job_failures = Family::<JobLabel, Counter>::default();
        registry.register(
            "background_job_failures",
            "Background job passes that failed or panicked, by job",
            job_failures.clone(),
        );
        let reconcile_refused = Counter::default();
        registry.register(
            "reconcile_removals_refused",
            "Reconcile passes whose removals were refused by the [reconcile] max_removals guard",
            reconcile_refused.clone(),
        );

        let retention_rows = Family::<KindLabel, Gauge>::default();
        registry.register(
            "retention_backlog_rows",
            "Rows past retention not yet removed, by kind — reported by the leader only",
            retention_rows.clone(),
        );
        let retention_event_age = Gauge::default();
        registry.register_with_unit(
            "retention_oldest_event_age",
            "Age of the oldest event row — compare with the event retention",
            prometheus_client::registry::Unit::Seconds,
            retention_event_age.clone(),
        );

        let (fanout_rows, fanout_age) = register_fanout(&mut registry);
        let feeds = register_feeds(&mut registry);
        let watermark_lag = Gauge::default();
        registry.register_with_unit(
            "event_watermark_lag",
            "How long the oldest committed event has been held back by a transaction still running",
            prometheus_client::registry::Unit::Seconds,
            watermark_lag.clone(),
        );

        Self {
            registry: Mutex::new(registry),
            published,
            published_channels: Mutex::new(std::collections::HashSet::new()),
            retention_rows,
            retention_event_age,
            fanout_rows,
            fanout_age,
            watermark_lag,
            push_duration,
            push_failures,
            sync_backlog,
            connections,
            dlq,
            job_success,
            job_failures,
            reconcile_refused,
            feeds,
        }
    }
}

/// RFC 0002 §10.1. No feed or channel label: feeds are unbounded in number and
/// their urls may be secret.
fn register_feeds(registry: &mut Registry) -> FeedMetrics {
    let polls = Family::<OutcomeLabel, Counter>::default();
    registry.register("feed_polls", "Feed polls, by how they ended", polls.clone());
    let items = Family::<OutcomeLabel, Counter>::default();
    registry.register(
        "feed_items",
        "Feed items published, folded into a summary, or skipped as invalid",
        items.clone(),
    );
    let failing = Gauge::default();
    registry.register(
        "feed_failing",
        "Feeds whose last poll failed — reported by the leader only",
        failing.clone(),
    );
    FeedMetrics {
        polls,
        items,
        failing,
    }
}

/// The fan-out outbox gauges (ADR-0025), on their own so `new` stays readable.
fn register_fanout(registry: &mut Registry) -> (Family<KindLabel, Gauge>, Gauge) {
    let rows = Family::<KindLabel, Gauge>::default();
    registry.register(
        "fanout_outbox_rows",
        "Fan-out outbox rows, pending or dead — reported by the leader only",
        rows.clone(),
    );
    let age = Gauge::default();
    registry.register_with_unit(
        "fanout_outbox_oldest_age",
        "Age of the oldest fan-out row still owed",
        prometheus_client::registry::Unit::Seconds,
        age.clone(),
    );
    (rows, age)
}

impl Metrics for PrometheusMetrics {
    fn published(&self, channel: &ChannelId) {
        let channel = {
            let mut seen = self
                .published_channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if seen.contains(channel.as_str()) || seen.len() < MAX_CHANNEL_SERIES {
                seen.insert(channel.to_string());
                channel.to_string()
            } else {
                OTHER_CHANNEL.to_owned()
            }
        };
        self.published
            .get_or_create(&ChannelLabel { channel })
            .inc();
    }

    fn retention_backlog(&self, backlog: &notify_ports::RetentionBacklog) {
        let gauge = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
        for (kind, rows) in [
            ("events", backlog.events_prunable),
            ("expire", backlog.pending_expiry),
            ("purge", backlog.pending_purge),
        ] {
            self.retention_rows
                .get_or_create(&KindLabel { kind })
                .set(gauge(rows));
        }
        self.retention_event_age
            .set(gauge(backlog.oldest_event_age_seconds));
    }

    fn fanout_backlog(&self, backlog: &notify_ports::FanoutBacklog) {
        let gauge = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
        for (kind, rows) in [("pending", backlog.pending), ("dead", backlog.dead)] {
            self.fanout_rows
                .get_or_create(&KindLabel { kind })
                .set(gauge(rows));
        }
        self.fanout_age
            .set(gauge(backlog.oldest_pending_age_seconds));
    }

    fn event_watermark_lag(&self, seconds: u64) {
        self.watermark_lag
            .set(i64::try_from(seconds).unwrap_or(i64::MAX));
    }

    fn push_finished(&self, transport: &'static str, result: PushResult, seconds: f64) {
        // Observed for every outcome, or latency graphs would improve during an outage.
        self.push_duration
            .get_or_create(&TransportLabel {
                transport: transport.to_owned(),
            })
            .observe(seconds);

        if let Some(reason) = result.reason() {
            self.push_failures
                .get_or_create(&FailureLabel {
                    transport: transport.to_owned(),
                    reason: reason.to_owned(),
                })
                .inc();
        }
    }

    fn sync_backlog(&self, events: usize) {
        // Lossy past 2^53 events, which is not a backlog anybody catches up on.
        #[expect(
            clippy::cast_precision_loss,
            reason = "a histogram observation, and the buckets top out at 1024"
        )]
        self.sync_backlog.observe(events as f64);
    }

    fn stream_opened(&self, transport: StreamKind) {
        self.connections
            .get_or_create(&TransportLabel {
                transport: transport.label().to_owned(),
            })
            .inc();
    }

    fn stream_closed(&self, transport: StreamKind) {
        self.connections
            .get_or_create(&TransportLabel {
                transport: transport.label().to_owned(),
            })
            .dec();
    }

    fn dlq_depth(&self, depths: &[(ChannelId, u64)]) {
        // Cleared first so a requeued channel stops being reported.
        self.dlq.clear();

        for (channel, depth) in depths {
            self.dlq
                .get_or_create(&ChannelLabel {
                    channel: channel.to_string(),
                })
                // Saturating: a negative queue would hide the bug.
                .set(i64::try_from(*depth).unwrap_or(i64::MAX));
        }
    }

    fn job_finished(&self, job: &'static str, ok: bool) {
        let label = JobLabel {
            task: job.to_owned(),
        };
        if ok {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs());
            self.job_success
                .get_or_create(&label)
                .set(i64::try_from(now).unwrap_or(i64::MAX));
        } else {
            self.job_failures.get_or_create(&label).inc();
        }
    }

    fn reconcile_removals_refused(&self) {
        self.reconcile_refused.inc();
    }

    fn feed_poll(&self, outcome: &'static str) {
        self.feeds
            .polls
            .get_or_create(&OutcomeLabel { outcome })
            .inc();
    }

    fn feed_items(&self, outcome: &'static str, count: u64) {
        self.feeds
            .items
            .get_or_create(&OutcomeLabel { outcome })
            .inc_by(count);
    }

    fn feed_failing(&self, feeds: u64) {
        self.feeds
            .failing
            .set(i64::try_from(feeds).unwrap_or(i64::MAX));
    }

    fn render(&self) -> String {
        let mut out = String::new();

        // Read through a poisoned lock rather than turn a scrape into a second outage.
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        #[expect(
            clippy::let_underscore_must_use,
            reason = "`encode` into a `String` cannot fail"
        )]
        let _ = encode(&mut out, &registry);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: &str) -> ChannelId {
        ChannelId::parse(id).unwrap()
    }

    #[test]
    fn a_publish_is_counted_per_channel_and_nothing_finer() {
        let metrics = PrometheusMetrics::new();
        metrics.published(&channel("ci-infra"));
        metrics.published(&channel("ci-infra"));
        metrics.published(&channel("cluster-ovh-1"));

        let rendered = metrics.render();
        assert!(rendered.contains("notifications_published_total{channel=\"ci-infra\"} 2"));
        assert!(rendered.contains("notifications_published_total{channel=\"cluster-ovh-1\"} 1"));
    }

    #[test]
    fn channels_past_the_cap_are_folded_into_other() {
        let metrics = PrometheusMetrics::new();
        for n in 0..MAX_CHANNEL_SERIES {
            metrics.published(&channel(&format!("c-{n}")));
        }
        metrics.published(&channel("one-too-many"));
        metrics.published(&channel("and-another"));
        // A channel that already had its series keeps it.
        metrics.published(&channel("c-0"));

        let rendered = metrics.render();
        assert!(rendered.contains("notifications_published_total{channel=\"other\"} 2"));
        assert!(rendered.contains("notifications_published_total{channel=\"c-0\"} 2"));
        assert!(!rendered.contains("one-too-many"));
    }

    #[test]
    fn feed_polls_and_items_are_counted_by_outcome_only() {
        let metrics = PrometheusMetrics::new();
        metrics.feed_poll("ok");
        metrics.feed_poll("ok");
        metrics.feed_poll("refused_address");
        metrics.feed_items("published", 3);
        metrics.feed_failing(2);

        let rendered = metrics.render();
        assert!(
            rendered.contains("feed_polls_total{outcome=\"ok\"} 2"),
            "{rendered}"
        );
        assert!(rendered.contains("feed_polls_total{outcome=\"refused_address\"} 1"));
        assert!(rendered.contains("feed_items_total{outcome=\"published\"} 3"));
        assert!(rendered.contains("feed_failing 2"));
    }

    #[test]
    fn the_fanout_outbox_is_exported() {
        let metrics = PrometheusMetrics::new();
        metrics.fanout_backlog(&notify_ports::FanoutBacklog {
            pending: 4,
            dead: 1,
            oldest_pending_age_seconds: 90,
        });

        let rendered = metrics.render();
        assert!(rendered.contains("fanout_outbox_rows{kind=\"pending\"} 4"));
        assert!(rendered.contains("fanout_outbox_rows{kind=\"dead\"} 1"));
        assert!(rendered.contains("fanout_outbox_oldest_age_seconds 90"));
    }

    #[test]
    fn the_watermark_lag_is_exported() {
        let metrics = PrometheusMetrics::new();
        metrics.event_watermark_lag(7);
        assert!(metrics.render().contains("event_watermark_lag_seconds 7"));
    }

    #[test]
    fn the_retention_backlog_is_exported() {
        let metrics = PrometheusMetrics::new();
        metrics.retention_backlog(&notify_ports::RetentionBacklog {
            oldest_event_age_seconds: 42,
            events_prunable: 3,
            pending_expiry: 2,
            pending_purge: 1,
        });
        let rendered = metrics.render();
        assert!(rendered.contains("retention_backlog_rows{kind=\"events\"} 3"));
        assert!(rendered.contains("retention_backlog_rows{kind=\"purge\"} 1"));
        assert!(rendered.contains("retention_oldest_event_age_seconds 42"));
    }

    #[test]
    fn a_push_is_timed_whether_or_not_it_arrived() {
        let metrics = PrometheusMetrics::new();
        metrics.push_finished("webpush", PushResult::Delivered, 0.2);
        metrics.push_finished("webpush", PushResult::Failed, 4.0);

        let rendered = metrics.render();
        assert!(
            rendered.contains("push_delivery_duration_seconds_count{transport=\"webpush\"} 2"),
            "a latency graph that improves during an outage is worse than none: {rendered}"
        );
        assert!(
            rendered.contains("push_failures_total{transport=\"webpush\",reason=\"failed\"} 1")
        );
        assert!(
            !rendered.contains("reason=\"delivered\""),
            "a delivery is not a failure"
        );
    }

    #[test]
    fn a_connection_that_closes_is_no_longer_held() {
        let metrics = PrometheusMetrics::new();
        metrics.stream_opened(StreamKind::Sse);
        metrics.stream_opened(StreamKind::Sse);
        metrics.stream_opened(StreamKind::WebSocket);
        metrics.stream_closed(StreamKind::Sse);

        let rendered = metrics.render();
        assert!(rendered.contains("realtime_connections{transport=\"sse\"} 1"));
        assert!(rendered.contains("realtime_connections{transport=\"ws\"} 1"));
    }

    #[test]
    fn an_emptied_queue_stops_being_reported_rather_than_reporting_its_last_depth() {
        let metrics = PrometheusMetrics::new();
        metrics.dlq_depth(&[(channel("ci-infra"), 3), (channel("cluster-ovh-1"), 1)]);
        assert!(
            metrics
                .render()
                .contains("sink_dlq_depth{channel=\"ci-infra\"} 3")
        );

        // An admin requeued everything on ci-infra.
        metrics.dlq_depth(&[(channel("cluster-ovh-1"), 1)]);

        let rendered = metrics.render();
        assert!(
            !rendered.contains("channel=\"ci-infra\""),
            "an alert must stop firing for a queue somebody emptied: {rendered}"
        );
        assert!(rendered.contains("sink_dlq_depth{channel=\"cluster-ovh-1\"} 1"));
    }

    #[test]
    fn a_labelled_family_appears_with_its_first_series_and_not_before() {
        // A fresh replica shows only the unlabelled `sync_backlog_size`; labelled families have no
        // series until recorded, so alerts need `absent()` or a `for:` window.
        let metrics = PrometheusMetrics::new();
        let fresh = metrics.render();

        assert!(fresh.contains("# HELP sync_backlog_size"));
        assert!(
            !fresh.contains("notifications_published"),
            "a labelled family with no series emits nothing: {fresh}"
        );

        metrics.published(&channel("ci-infra"));
        assert!(
            metrics.render().contains("# HELP notifications_published"),
            "and appears with its first series"
        );
    }

    #[test]
    fn a_job_reports_when_it_last_worked_and_how_often_it_did_not() {
        let metrics = PrometheusMetrics::new();
        metrics.job_finished("purge", true);
        metrics.job_finished("sink-drain", false);
        metrics.job_finished("sink-drain", false);
        metrics.reconcile_removals_refused();

        let rendered = metrics.render();
        assert!(
            rendered.contains("background_job_last_success_timestamp_seconds{task=\"purge\"} 1"),
            "{rendered}"
        );
        assert!(rendered.contains("background_job_failures_total{task=\"sink-drain\"} 2"));
        assert!(rendered.contains("reconcile_removals_refused_total 1"));
    }

    #[test]
    fn every_name_design_13_publishes_is_the_name_this_registry_uses() {
        // Alert rules are written against these names: renaming is a breaking change.
        let metrics = PrometheusMetrics::new();
        metrics.published(&channel("ci-infra"));
        metrics.push_finished("webpush", PushResult::Failed, 0.1);
        metrics.sync_backlog(3);
        metrics.stream_opened(StreamKind::Sse);
        metrics.dlq_depth(&[(channel("ci-infra"), 1)]);

        let rendered = metrics.render();
        for family in [
            "notifications_published_total",
            "push_delivery_duration_seconds",
            "push_failures_total",
            "sync_backlog_size",
            "realtime_connections",
            "sink_dlq_depth",
        ] {
            assert!(rendered.contains(family), "{family} is missing: {rendered}");
        }
    }
}
