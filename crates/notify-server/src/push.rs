//! Which push transports this instance actually has (design §7.1, §11).
//!
//! The [`PushDispatcher`] port exists so nothing above it knows which transports
//! were wired: no APNs key means no APNs, and that is a configuration answer rather
//! than a code path. This is the registry that answers it.
//!
//! # An empty registry is a deployment's answer, not a stub
//!
//! Web Push is here since M3.5, UnifiedPush lands in M6 and APNs in M7 — but a
//! deployment with no `[push.webpush]` still gets an empty registry, and the
//! delivery path is complete without one: a device enrolled with
//! [`notify_domain::PushTransport::Stream`] is reachable only while its client holds
//! a stream, and the event-bus poke already covered it before the dispatcher was
//! consulted (`Fanout::push` skips it).
//!
//! So the registry is reached only by a device that enrolled with a transport this
//! instance does not run. Answering [`PortError::NotConfigured`] names the missing
//! capability, the fan-out logs it and carries on, and the publish never fails —
//! a CI job must not go red because somebody's phone chose a transport this
//! deployment did not configure.

use std::collections::BTreeMap;
use std::sync::Arc;

use notify_domain::Device;
use notify_ports::{
    PortError, PortResult, PushDispatcher, PushMessage, PushOutcome, PushTransport,
};

/// The transports this deployment has, keyed by the name a device enrols under.
#[derive(Clone, Default)]
pub(crate) struct TransportRegistry {
    transports: BTreeMap<&'static str, Arc<dyn PushTransport>>,
    /// Where `push_delivery_duration_seconds` and `push_failures_total` land
    /// (design §13).
    ///
    /// Here rather than in the fan-out because this is the only place that knows
    /// *which transport* answered — the fan-out holds a `dyn PushDispatcher` and is
    /// deliberately unable to tell them apart.
    metrics: Option<Arc<dyn notify_ports::Metrics>>,
}

impl core::fmt::Debug for TransportRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TransportRegistry")
            .field("configured", &self.names())
            .finish()
    }
}

impl TransportRegistry {
    /// An instance with no push transports.
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Register one, replacing any transport already under that name.
    pub(crate) fn with(mut self, transport: Arc<dyn PushTransport>) -> Self {
        self.transports.insert(transport.name(), transport);
        self
    }

    /// Report every attempt to the exporter (design §13).
    #[must_use]
    pub(crate) fn measured_by(mut self, metrics: Arc<dyn notify_ports::Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// What is configured, for the startup log and for `/readyz`.
    pub(crate) fn names(&self) -> Vec<&'static str> {
        self.transports.keys().copied().collect()
    }

    /// The largest payload any configured transport will carry.
    ///
    /// The fan-out degrades to a poke past this. Zero when nothing is configured,
    /// which is right: with no transport there is no payload that fits.
    pub(crate) fn max_payload_bytes(&self) -> usize {
        self.transports
            .values()
            .map(|transport| transport.max_payload_bytes())
            .max()
            .unwrap_or(0)
    }
}

#[async_trait::async_trait]
impl PushDispatcher for TransportRegistry {
    /// The last span of §13's `publish → fanout → push transport`.
    ///
    /// Named by the transport and not by the device: which relay was slow is the
    /// question a trace is opened to answer, and a device id in a span attribute
    /// would put somebody's endpoint in a document an operator reads (§13 — the
    /// same rule the metrics labels follow).
    #[tracing::instrument(name = "push", skip_all, fields(transport = device.transport.name()))]
    async fn dispatch(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        let wanted = device.transport.name();

        let Some(transport) = self.transports.get(wanted) else {
            // Named rather than generic, so an operator reading the log is told
            // which capability to configure instead of that "push failed".
            self.record(wanted, notify_ports::PushResult::Unusable, 0.0);
            return Err(PortError::NotConfigured {
                what: capability(wanted),
            });
        };

        let started = std::time::Instant::now();
        let outcome = transport.deliver(device, message).await;

        let result = match &outcome {
            Ok(PushOutcome::Delivered) => notify_ports::PushResult::Delivered,
            Ok(PushOutcome::Expired) => notify_ports::PushResult::Expired,
            Ok(PushOutcome::Failed { .. }) => notify_ports::PushResult::Failed,
            Err(_) => notify_ports::PushResult::Unusable,
        };
        self.record(transport.name(), result, started.elapsed().as_secs_f64());

        outcome
    }
}

impl TransportRegistry {
    /// Hand one attempt to the exporter, if there is one.
    fn record(&self, transport: &'static str, result: notify_ports::PushResult, seconds: f64) {
        if let Some(metrics) = &self.metrics {
            metrics.push_finished(transport, result, seconds);
        }
    }
}

/// The `push.<transport>` capability name, as `/readyz` and the metrics spell it.
///
/// A closed match rather than a leaked `format!`: these are metric labels and
/// `PortError::NotConfigured` holds a `&'static str`, so the set has to be bounded.
const fn capability(transport: &str) -> &'static str {
    match transport.as_bytes() {
        b"webpush" => "push.webpush",
        b"unifiedpush" => "push.unifiedpush",
        b"apns" => "push.apns",
        // `stream` never reaches here — the fan-out skips it — and anything else is
        // a transport this build does not know.
        _ => "push",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing assertion is a failing test")]

    use notify_domain::{ChannelId, DeviceId, NotificationId, Principal, Priority, UserId};
    use notify_ports::PushBody;
    use time::OffsetDateTime;

    use super::*;

    fn device(transport: notify_domain::PushTransport) -> Device {
        Device::enrol(
            DeviceId::generate(),
            Principal::User {
                id: UserId::generate(),
            },
            "phone",
            transport,
            None,
            OffsetDateTime::now_utc(),
        )
        .unwrap()
    }

    fn message() -> PushMessage {
        PushMessage::Notification {
            channel: ChannelId::parse("ci-infra").unwrap(),
            notification_id: NotificationId::generate(),
            priority: Priority::Default,
            body: PushBody::Poke,
        }
    }

    #[tokio::test]
    async fn a_transport_this_instance_does_not_run_is_named_rather_than_blamed() {
        let registry = TransportRegistry::empty();
        let device = device(notify_domain::PushTransport::WebPush {
            endpoint: "https://push.example/abc".parse().unwrap(),
            p256dh: "BPk".to_owned(),
            auth: "k9".to_owned(),
        });

        let error = registry.dispatch(&device, &message()).await.unwrap_err();

        assert!(matches!(
            error,
            PortError::NotConfigured {
                what: "push.webpush"
            }
        ));
        assert!(
            !error.is_retryable(),
            "retrying a missing configuration is a busy loop; an operator has to act"
        );
    }

    #[test]
    fn an_instance_with_no_transports_carries_no_payload() {
        let registry = TransportRegistry::empty();

        assert!(registry.names().is_empty());
        assert_eq!(
            registry.max_payload_bytes(),
            0,
            "with no transport there is no payload that fits, so everything degrades \
             to a poke"
        );
    }

    #[test]
    fn every_transport_a_device_can_enrol_with_maps_to_a_capability_name() {
        // These are what `/readyz` and `push_failures_total{reason}` report, so an
        // unnamed one would show up as a blank label.
        assert_eq!(capability("webpush"), "push.webpush");
        assert_eq!(capability("unifiedpush"), "push.unifiedpush");
        assert_eq!(capability("apns"), "push.apns");
    }
}
