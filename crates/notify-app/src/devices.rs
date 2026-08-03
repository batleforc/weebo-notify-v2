//! Devices: enrolling, listing and revoking (design §8).
//!
//! M1 needs devices for one reason — a sync cursor has to belong to something — so
//! this is the minimum that supports the stream and the backlog. Push transports
//! are enrollable from the start because their invariant (a relayed transport must
//! arrive with a key) belongs at enrolment rather than at the first delivery.

use std::sync::Arc;

use notify_domain::{Device, DeviceId, DevicePublicKey, PushTransport};
use notify_ports::{AuditEntry, AuditLog, Clock, DeviceRepository, Failure};

use crate::authz::Caller;

/// `POST /api/v1/devices` (design §8).
#[derive(Clone)]
pub struct EnrolDevice {
    devices: Arc<dyn DeviceRepository>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for EnrolDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EnrolDevice").finish_non_exhaustive()
    }
}

impl EnrolDevice {
    /// Wire it.
    #[must_use]
    pub fn new(
        devices: Arc<dyn DeviceRepository>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            devices,
            audit,
            clock,
        }
    }

    /// Enrol a device for the calling principal.
    ///
    /// The owner is taken from the caller, never from the request: enrolling a
    /// device "for" somebody else would be a way to point their notifications at
    /// your own push endpoint.
    pub async fn execute(
        &self,
        caller: &Caller,
        label: &str,
        transport: PushTransport,
        public_key: Option<DevicePublicKey>,
    ) -> Result<Device, Failure> {
        let now = self.clock.now();
        let device = Device::enrol(
            DeviceId::generate(),
            caller.principal.clone(),
            label,
            transport,
            public_key,
            now,
        )?;

        self.devices.upsert(&device).await?;
        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone(),
                action: "device.enrolled",
                target: Some(device.id.to_string()),
                channel: None,
                detail: serde_json::to_value(device.transport.name()).ok(),
                at: now,
            })
            .await?;

        Ok(device)
    }
}

/// `GET /api/v1/devices` (design §8).
#[derive(Clone)]
pub struct ListDevices {
    devices: Arc<dyn DeviceRepository>,
}

impl core::fmt::Debug for ListDevices {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ListDevices").finish_non_exhaustive()
    }
}

impl ListDevices {
    /// Wire it.
    #[must_use]
    pub fn new(devices: Arc<dyn DeviceRepository>) -> Self {
        Self { devices }
    }

    /// The caller's own devices, revoked ones included so a device list can show
    /// what was revoked and when.
    pub async fn execute(&self, caller: &Caller) -> Result<Vec<Device>, Failure> {
        Ok(self.devices.list_for(&caller.principal).await?)
    }
}

/// `DELETE /api/v1/devices/{id}` (design §8, §15, scenario T18).
#[derive(Clone)]
pub struct RevokeDevice {
    devices: Arc<dyn DeviceRepository>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for RevokeDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RevokeDevice").finish_non_exhaustive()
    }
}

impl RevokeDevice {
    /// Wire it.
    #[must_use]
    pub fn new(
        devices: Arc<dyn DeviceRepository>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            devices,
            audit,
            clock,
        }
    }

    /// Revoke one of the caller's own devices: subscription, cursor and key all go.
    pub async fn execute(&self, caller: &Caller, id: DeviceId) -> Result<bool, Failure> {
        let Some(device) = self.devices.get(id).await? else {
            return Ok(false);
        };
        if device.owner != caller.principal {
            // Same answer as "no such device": a stranger does not get to enumerate
            // somebody else's hardware.
            return Ok(false);
        }

        let now = self.clock.now();
        let revoked = self.devices.revoke(id, now).await?;
        if revoked {
            self.audit
                .record(&AuditEntry {
                    actor: caller.principal.clone(),
                    action: "device.revoked",
                    target: Some(id.to_string()),
                    channel: None,
                    detail: None,
                    at: now,
                })
                .await?;
        }
        Ok(revoked)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{Cursor, Principal, UserId};
    use notify_ports::test_utils::{InMemory, TestClock};
    use url::Url;

    use super::*;

    struct Fixture {
        store: InMemory,
        clock: TestClock,
        enrol: EnrolDevice,
        list: ListDevices,
        revoke: RevokeDevice,
        alice: Principal,
    }

    fn fixture() -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();

        Fixture {
            enrol: EnrolDevice::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            list: ListDevices::new(Arc::new(store.clone())),
            revoke: RevokeDevice::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            store,
            clock,
            alice: Principal::User {
                id: UserId::generate(),
            },
        }
    }

    fn webpush() -> PushTransport {
        PushTransport::WebPush {
            endpoint: Url::parse("https://push.example/abc").unwrap(),
            p256dh: "BPk".to_owned(),
            auth: "k9".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_device_is_always_enrolled_for_the_caller() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());

        let device = fixture
            .enrol
            .execute(&caller, "laptop", PushTransport::Stream, None)
            .await
            .unwrap();

        assert_eq!(device.owner, fixture.alice);
        assert_eq!(fixture.list.execute(&caller).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_relayed_transport_without_a_key_is_refused_at_enrolment() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());

        assert!(
            fixture
                .enrol
                .execute(
                    &caller,
                    "phone",
                    PushTransport::Apns {
                        token: "abc".to_owned()
                    },
                    None,
                )
                .await
                .is_err(),
            "a device we cannot seal to should fail now, not at the first incident"
        );
    }

    // --- T18 · Revocation (design §3.4) -----------------------------------

    #[tokio::test]
    async fn critical_t18_revoking_drops_the_subscription_and_the_cursor() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        let device = fixture
            .enrol
            .execute(&caller, "phone", webpush(), None)
            .await
            .unwrap();

        DeviceRepository::advance_cursor(
            &fixture.store,
            device.id,
            Cursor::from_sequence(42),
            fixture.clock.now(),
        )
        .await
        .unwrap();

        assert!(fixture.revoke.execute(&caller, device.id).await.unwrap());

        let stored = DeviceRepository::get(&fixture.store, device.id)
            .await
            .unwrap()
            .unwrap();
        assert!(!stored.is_active());
        assert_eq!(stored.transport, PushTransport::Stream);
        assert_eq!(stored.cursor, None);
        assert!(
            fixture
                .store
                .audit_entries()
                .iter()
                .any(|entry| entry.action == "device.revoked")
        );
    }

    #[tokio::test]
    async fn you_cannot_revoke_somebody_elses_device() {
        let fixture = fixture();
        let device = fixture
            .enrol
            .execute(
                &Caller::member(fixture.alice.clone()),
                "laptop",
                PushTransport::Stream,
                None,
            )
            .await
            .unwrap();

        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });
        assert!(!fixture.revoke.execute(&stranger, device.id).await.unwrap());
        assert!(
            DeviceRepository::get(&fixture.store, device.id)
                .await
                .unwrap()
                .unwrap()
                .is_active()
        );
    }
}
