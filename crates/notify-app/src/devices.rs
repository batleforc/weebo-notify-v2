//! Devices: enrolling, listing and revoking (§8). A relayed push transport must
//! arrive with a key, checked at enrolment.

use std::sync::Arc;

use notify_domain::{Device, DeviceId, DevicePublicKey, PushTransport};
use notify_ports::{AuditEntry, AuditLog, Clock, DeviceRepository, Failure, actions};

use crate::authz::{Caller, Grant};

/// Whether `caller` may enrol, see or revoke a device on `transport`.
///
/// A session reaches every device. Anything else — the CLI's personal key, a CI
/// token — reaches only `stream` ones: those deliver nothing on their own, the
/// stream they carry is scoped by the credential that opens it, and they are all
/// the CLI enrols (`notify device enrol`). A push transport is the one that matters:
/// one enrolled from a leaked key would receive everything its owner does, and
/// listing them hands out the owner's push endpoints.
fn reaches(caller: &Caller, transport: &PushTransport) -> bool {
    caller.ensure_session().is_ok() || matches!(transport, PushTransport::Stream)
}

/// The active devices one owner may hold unless `[devices] max_per_user` says
/// otherwise.
pub const DEFAULT_MAX_DEVICES_PER_USER: u32 = 20;

/// `POST /api/v1/devices` (design §8).
#[derive(Clone)]
pub struct EnrolDevice {
    devices: Arc<dyn DeviceRepository>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
    max_per_user: u32,
}

impl core::fmt::Debug for EnrolDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EnrolDevice").finish_non_exhaustive()
    }
}

impl EnrolDevice {
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
            max_per_user: DEFAULT_MAX_DEVICES_PER_USER,
        }
    }

    /// Cap each owner at `max` active devices (`[devices] max_per_user`).
    #[must_use]
    pub const fn with_max_per_user(mut self, max: u32) -> Self {
        self.max_per_user = max;
        self
    }

    /// Enrol a device for the calling principal; the owner always comes from the caller.
    /// Only a session enrols a push device (see [`reaches`]).
    pub async fn execute(
        &self,
        caller: &Caller,
        label: &str,
        transport: PushTransport,
        public_key: Option<DevicePublicKey>,
    ) -> Result<Device, Failure> {
        if !reaches(caller, &transport) {
            return Err(notify_domain::DomainError::SessionRequired.into());
        }

        let now = self.clock.now();
        let device = Device::enrol(
            DeviceId::generate(),
            caller.principal.clone(),
            label,
            transport,
            public_key,
            now,
        )?;

        // Every active device counts, push and stream alike: a cap on push alone would
        // let a leaked key enrol stream devices without end.
        if !self
            .devices
            .enrol_capped(&device, self.max_per_user)
            .await?
        {
            return Err(notify_domain::DomainError::DeviceLimitReached {
                max: self.max_per_user,
            }
            .into());
        }
        // Record the owner's groups so group-only bindings reach this device offline (§11).
        // Membership only: a key's empty groups would erase the snapshot.
        if caller.grant == Grant::Membership {
            self.devices
                .remember_groups(&caller.principal, &caller.groups, now)
                .await?;
        }
        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone().into(),
                action: actions::DEVICE_ENROLLED,
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
    #[must_use]
    pub fn new(devices: Arc<dyn DeviceRepository>) -> Self {
        Self { devices }
    }

    /// The caller's own devices, revoked ones included; only `stream` ones for a key
    /// (see [`reaches`]).
    pub async fn execute(&self, caller: &Caller) -> Result<Vec<Device>, Failure> {
        let mut devices = self.devices.list_for(&caller.principal).await?;
        devices.retain(|device| reaches(caller, &device.transport));
        Ok(devices)
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
            // Same answer as "no such device".
            return Ok(false);
        }
        if !reaches(caller, &device.transport) {
            // After the ownership check, so this says nothing about anybody else's
            // devices: the caller owns it and should learn why.
            return Err(notify_domain::DomainError::SessionRequired.into());
        }

        let now = self.clock.now();
        let revoked = self.devices.revoke(id, now).await?;
        if revoked {
            self.audit
                .record(&AuditEntry {
                    actor: caller.principal.clone().into(),
                    action: actions::DEVICE_REVOKED,
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
    async fn enrolment_past_the_per_user_cap_is_refused_until_one_is_revoked() {
        let fixture = fixture();
        let enrol = fixture.enrol.clone().with_max_per_user(2);
        let caller = Caller::member(fixture.alice.clone());

        let first = enrol
            .execute(&caller, "laptop", PushTransport::Stream, None)
            .await
            .unwrap();
        enrol
            .execute(&caller, "phone", webpush(), None)
            .await
            .unwrap();

        let refused = enrol
            .execute(&caller, "tablet", PushTransport::Stream, None)
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, Failure::Domain(error) if error.code() == "device.limit_reached"),
            "a limit with a stable code, not an outage: {refused:?}"
        );

        // Somebody else's devices are not this owner's count.
        let bob = Caller::member(Principal::User {
            id: UserId::generate(),
        });
        enrol
            .execute(&bob, "laptop", PushTransport::Stream, None)
            .await
            .unwrap();

        // A revoked device frees its slot.
        fixture.revoke.execute(&caller, first.id).await.unwrap();
        enrol
            .execute(&caller, "tablet", PushTransport::Stream, None)
            .await
            .unwrap();
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
    async fn enrolling_remembers_the_callers_groups_so_a_group_bound_channel_can_reach_them() {
        use notify_domain::{Channel, GroupBinding, Scope};

        let fixture = fixture();
        let mut channel = Channel::dynamic(
            notify_domain::ChannelId::parse("cluster-ovh-1").unwrap(),
            None,
        )
        .unwrap();
        // Only the group grants alice access.
        channel.upsert_group(GroupBinding::new("infra-admins", vec![Scope::Subscribe]).unwrap());
        fixture.store.seed_channel(channel);

        let caller = Caller::member_of(fixture.alice.clone(), vec!["infra-admins".to_owned()]);

        assert!(
            DeviceRepository::delivery_targets(
                &fixture.store,
                &notify_domain::ChannelId::parse("cluster-ovh-1").unwrap()
            )
            .await
            .unwrap()
            .is_empty(),
            "nobody has enrolled yet"
        );

        fixture
            .enrol
            .execute(&caller, "laptop", webpush(), None)
            .await
            .unwrap();

        let targets = DeviceRepository::delivery_targets(
            &fixture.store,
            &notify_domain::ChannelId::parse("cluster-ovh-1").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            targets.len(),
            1,
            "enrolling should have remembered alice's groups, reaching her through \
             the channel's group binding rather than an individual membership"
        );
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
                .any(|entry| entry.action.as_str() == "device.revoked")
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
