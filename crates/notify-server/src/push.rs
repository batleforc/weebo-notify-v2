//! Which push transports this instance actually has (design §7.1, §11).
//!
//! The [`PushDispatcher`] port exists so nothing above it knows which transports
//! were wired: no APNs key means no APNs, and that is a configuration answer rather
//! than a code path. This is the registry that answers it.
//!
//! # M1 has no transports, and that is the correct behaviour
//!
//! Web Push lands in M3, UnifiedPush in M6, APNs in M7. Until then the registry is
//! empty — which is not a stub, because the M1 delivery path is complete without
//! one: a device enrolled with [`notify_domain::PushTransport::Stream`] is reachable
//! only while its client holds a stream, and the event-bus poke already covered it
//! before the dispatcher was consulted (`Fanout::push` skips it).
//!
//! So the registry is reached only by a device that enrolled with a transport this
//! instance cannot serve. Answering [`PortError::NotConfigured`] names the missing
//! capability, the fan-out logs it and carries on, and the publish never fails —
//! a CI job must not go red because somebody's phone chose a transport we do not
//! run yet.

// Same reason as `config`: the caller is the composition root's `run`, which is
// blocked on group-based channel access. The registry itself is complete.
#![allow(
    dead_code,
    reason = "the caller of these is the composition root, which is blocked"
)]

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
}

impl core::fmt::Debug for TransportRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TransportRegistry")
            .field("configured", &self.names())
            .finish()
    }
}

impl TransportRegistry {
    /// An instance with no push transports — every M1 deployment.
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Register one, replacing any transport already under that name.
    #[allow(
        dead_code,
        reason = "the first caller is the Web Push transport in M3; the registry \
                  exists now because the composition root needs a dispatcher now"
    )]
    pub(crate) fn with(mut self, transport: Arc<dyn PushTransport>) -> Self {
        self.transports.insert(transport.name(), transport);
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
    async fn dispatch(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        let wanted = device.transport.name();

        let Some(transport) = self.transports.get(wanted) else {
            // Named rather than generic, so an operator reading the log is told
            // which capability to configure instead of that "push failed".
            return Err(PortError::NotConfigured {
                what: capability(wanted),
            });
        };

        transport.deliver(device, message).await
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
