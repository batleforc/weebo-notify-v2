//! Devices and their sync cursors.

use notify_domain::{ChannelId, Cursor, Device, DeviceId, Principal};
use time::OffsetDateTime;

use crate::error::PortResult;

/// Devices.
#[async_trait::async_trait]
pub trait DeviceRepository: Send + Sync {
    /// Fetch one.
    async fn get(&self, id: DeviceId) -> PortResult<Option<Device>>;

    /// A principal's devices, revoked ones included (§8, `GET /devices`).
    async fn list_for(&self, owner: &Principal) -> PortResult<Vec<Device>>;

    /// Enrol or update a device.
    async fn upsert(&self, device: &Device) -> PortResult<()>;

    /// Enrol a device unless its owner already holds `max` active ones (`false`,
    /// nothing written). Count and insert are atomic per owner.
    async fn enrol_capped(&self, device: &Device, max: u32) -> PortResult<bool>;

    /// Revoke a device: push subscription, cursor and public key all go (§8, §15, T18).
    /// Returns whether one was there to revoke.
    async fn revoke(&self, id: DeviceId, at: OffsetDateTime) -> PortResult<bool>;

    /// Record how far a device has read. Monotonic, so an older tab cannot
    /// re-deliver what was already seen.
    async fn advance_cursor(
        &self,
        id: DeviceId,
        cursor: Cursor,
        at: OffsetDateTime,
    ) -> PortResult<()>;

    /// The active devices of every member with `subscribe`, direct or via a group
    /// snapshot. Silence/DND filtering is the use case's job (§7.4).
    async fn delivery_targets(&self, channel: &ChannelId) -> PortResult<Vec<Device>>;

    /// Snapshot the groups a principal's token carried, so `delivery_targets` can
    /// reach them through group-only bindings while offline. The only place group
    /// membership is persisted (§11), refreshed at enrolment and sync start.
    async fn remember_groups(
        &self,
        principal: &Principal,
        groups: &[String],
        at: OffsetDateTime,
    ) -> PortResult<()>;

    /// The groups last remembered for `principal`, empty if none or older than
    /// `since`; caps token-less personal API keys.
    async fn remembered_groups(
        &self,
        principal: &Principal,
        since: OffsetDateTime,
    ) -> PortResult<Vec<String>>;
}
