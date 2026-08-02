//! Devices and their sync cursors.

use notify_domain::{ChannelId, Cursor, Device, DeviceId, Principal};
use time::OffsetDateTime;

use crate::error::PortResult;

/// Devices.
#[async_trait::async_trait]
pub trait DeviceRepository: Send + Sync {
    /// Fetch one.
    async fn get(&self, id: DeviceId) -> PortResult<Option<Device>>;

    /// A principal's devices (design §8, `GET /devices`).
    ///
    /// Includes revoked ones so a device list can show what was revoked and when;
    /// delivery filters them out itself.
    async fn list_for(&self, owner: &Principal) -> PortResult<Vec<Device>>;

    /// Enrol or update a device.
    async fn upsert(&self, device: &Device) -> PortResult<()>;

    /// Revoke a device: push subscription, cursor and public key all go
    /// (design §8, §15, scenario T18). Returns whether one was there to revoke.
    async fn revoke(&self, id: DeviceId, at: OffsetDateTime) -> PortResult<bool>;

    /// Record how far a device has read.
    ///
    /// Monotonic, and enforced here as well as in the domain: two tabs of the same
    /// client acknowledging different positions must not let the older one win and
    /// re-deliver what was already seen.
    async fn advance_cursor(
        &self,
        id: DeviceId,
        cursor: Cursor,
        at: OffsetDateTime,
    ) -> PortResult<()>;

    /// The active devices to deliver a channel's notification to.
    ///
    /// Returns the devices of every member with `subscribe`, which is what the
    /// fan-out iterates. Silenced and do-not-disturb recipients are filtered by the
    /// use case, not here: this port answers "who could be reached", the use case
    /// answers "who should be" (design §7.4).
    async fn delivery_targets(&self, channel: &ChannelId) -> PortResult<Vec<Device>>;
}
