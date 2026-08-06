//! The registry behind `GET /metrics` (design §13).
//!
//! Six families, named by §13 and not by this file — they end up in dashboards and
//! alert rules that live outside this repository, so renaming one breaks somebody
//! else's pager.
//!
//! # What carries a label, and what does not
//!
//! A label multiplies series, and a label whose values are unbounded multiplies
//! them without end — the classic way a metrics endpoint becomes the heaviest thing
//! in a process. Two decisions follow:
//!
//! * **`channel` is a label** (`notifications_published_total`, `sink_dlq_depth`)
//!   because channels are declared, few, and the dimension an operator actually
//!   asks about;
//! * **notification ids, user ids and endpoints are not**, anywhere. They are
//!   unbounded, and the second one would put somebody's identity in a document
//!   this endpoint serves to whoever can reach it.
//!
//! # The dead-letter depth is a leader's answer
//!
//! [`Metrics::dlq_depth`] replaces the whole family rather than adding to it, and
//! the drain loop is the only caller — so on a replica that is not leading, the
//! family stays **empty** rather than reporting zero. An absent series and a series
//! reading zero are different claims: the first says "this replica does not know",
//! the second says "nothing is stuck", and only one of them is true on a follower.

use std::sync::Mutex;

use notify_domain::ChannelId;
use notify_ports::{Metrics, PushResult, StreamKind};
use prometheus_client::encoding::{EncodeLabelSet, text::encode};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

/// `{channel="ci-infra"}`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ChannelLabel {
    channel: String,
}

/// `{transport="webpush"}`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct TransportLabel {
    transport: String,
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
    /// Guarded because `dlq_depth` clears a family, which the lock-free families
    /// alone cannot express — and because rendering has to see a consistent set.
    registry: Mutex<Registry>,
    published: Family<ChannelLabel, Counter>,
    push_duration: Family<TransportLabel, Histogram>,
    push_failures: Family<FailureLabel, Counter>,
    sync_backlog: Histogram,
    connections: Family<TransportLabel, Gauge>,
    dlq: Family<ChannelLabel, Gauge>,
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

        // Buckets from a relay answering quickly to one that has stopped answering:
        // the default Prometheus set tops out at ten seconds, and the transports
        // here time out well before that.
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

        // Powers of four rather than of two: what matters is the order of magnitude
        // of a catch-up — a client behind by four events and one behind by five are
        // the same operational fact.
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

        Self {
            registry: Mutex::new(registry),
            published,
            push_duration,
            push_failures,
            sync_backlog,
            connections,
            dlq,
        }
    }
}

impl Metrics for PrometheusMetrics {
    fn published(&self, channel: &ChannelId) {
        self.published
            .get_or_create(&ChannelLabel {
                channel: channel.to_string(),
            })
            .inc();
    }

    fn push_finished(&self, transport: &'static str, result: PushResult, seconds: f64) {
        // Observed whatever the outcome: a transport that only ever appears in the
        // duration histogram when it succeeds makes a latency graph that improves
        // during an outage.
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
        // Cleared first: a channel whose dead letters were requeued must stop being
        // reported, not keep its last value until a restart.
        self.dlq.clear();

        for (channel, depth) in depths {
            self.dlq
                .get_or_create(&ChannelLabel {
                    channel: channel.to_string(),
                })
                // Saturating rather than wrapping: a depth past `i64::MAX` is a bug
                // somewhere else, and reporting a negative queue would hide it.
                .set(i64::try_from(*depth).unwrap_or(i64::MAX));
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();

        // A poisoned lock means a previous render panicked, which nothing here can
        // do. Reading through it keeps the endpoint answering rather than turning a
        // metrics scrape into a second outage.
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // The only failure `encode` reports is a formatting one into a `String`,
        // which cannot happen; an empty body is still a valid scrape.
        let _ = encode(&mut out, &registry);
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

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
        // Worth a test because it is the first surprise an operator meets: a scrape
        // of a freshly started replica shows `sync_backlog_size` — which has no
        // labels and therefore one series from the start — and *nothing else*. A
        // labelled family has no series until something is recorded, so a dashboard
        // panel reads "no data" on an idle instance rather than zero.
        //
        // That is `prometheus-client`'s model and not a choice made here; the way to
        // make an alert survive it is `absent()` or a `for:` window, which is worth
        // knowing before writing the alert rather than after.
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
    fn every_name_design_13_publishes_is_the_name_this_registry_uses() {
        // The six strings alert rules are written against. A rename is a breaking
        // change for somebody outside this repository, so it fails here first.
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
