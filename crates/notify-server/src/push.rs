//! Which push transports this instance has (design §7.1, §11). A device on an unconfigured
//! transport gets [`PortError::NotConfigured`]: logged by the fan-out, never failing the publish.

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
    /// Where push duration and failure metrics land (design §13); only the registry knows the transport.
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

    /// The largest content any transport could carry (zero with none): the fan-out's first cut.
    /// Each transport still measures its own wire body against its own limit and pokes when over.
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
    /// The last span of §13's `publish → fanout → push transport`, named by transport, never device.
    #[tracing::instrument(name = "push", skip_all, fields(transport = device.transport.name()))]
    async fn dispatch(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        let wanted = device.transport.name();

        let Some(transport) = self.transports.get(wanted) else {
            // Named, so the operator knows which capability to configure.
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

/// The `push.<transport>` capability name; a closed match since metric labels must be bounded.
const fn capability(transport: &str) -> &'static str {
    match transport.as_bytes() {
        b"webpush" => "push.webpush",
        b"unifiedpush" => "push.unifiedpush",
        b"apns" => "push.apns",
        // `stream` never reaches here; anything else is unknown to this build.
        _ => "push",
    }
}

#[cfg(test)]
mod tests {
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

        assert_eq!(registry.names(), [] as [&str; 0]);
        assert_eq!(
            registry.max_payload_bytes(),
            0,
            "with no transport there is no payload that fits, so everything degrades \
             to a poke"
        );
    }

    #[test]
    fn every_transport_a_device_can_enrol_with_maps_to_a_capability_name() {
        // Shown by `/readyz` and as a metric label, so none may be blank.
        assert_eq!(capability("webpush"), "push.webpush");
        assert_eq!(capability("unifiedpush"), "push.unifiedpush");
        assert_eq!(capability("apns"), "push.apns");
    }
}
