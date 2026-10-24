//! Channels, their members and their policy.

use notify_domain::{Channel, ChannelId, ChannelPolicy, Membership, Principal, Scope};

use crate::error::PortResult;
use crate::pagination::{Page, PageRequest};

/// Channels. Reads take groups as well as a principal because group bindings
/// have no per-user row (§5.1, §11); forgetting them empties group-based inboxes.
#[async_trait::async_trait]
pub trait ChannelRepository: Send + Sync {
    /// Fetch one by name.
    async fn get(&self, id: &ChannelId) -> PortResult<Option<Channel>>;

    /// The channels a caller holds at least `scope` in, by membership or group (§5.4).
    async fn list_for(
        &self,
        principal: &Principal,
        groups: &[String],
        scope: Scope,
        page: PageRequest,
    ) -> PortResult<Page<Channel>>;

    /// Every channel id a caller may read, unpaginated: stream filters must be complete.
    async fn subscribed_ids(
        &self,
        principal: &Principal,
        groups: &[String],
    ) -> PortResult<Vec<ChannelId>>;

    /// Create or update a channel, members and policy included. Refusing to reshape
    /// managed channels is the use case's job (§11).
    async fn upsert(&self, channel: &Channel) -> PortResult<()>;

    /// Create a channel that must not exist yet; [`crate::PortError::Conflict`] if it
    /// does, checked in the same write to avoid a race.
    async fn insert(&self, channel: &Channel) -> PortResult<()>;

    /// Add one membership or change its scopes, leaving the others alone (no
    /// lost update between concurrent managers).
    async fn put_member(&self, id: &ChannelId, membership: &Membership) -> PortResult<()>;

    /// Remove one membership. Returns whether it was there.
    async fn remove_member(&self, id: &ChannelId, principal: &Principal) -> PortResult<bool>;

    /// Replace the policy, leaving members and group access alone.
    async fn set_policy(&self, id: &ChannelId, policy: &ChannelPolicy) -> PortResult<()>;

    /// Delete a channel. Returns whether one was there.
    async fn delete(&self, id: &ChannelId) -> PortResult<bool>;

    /// Every GitOps-managed channel, for reconciliation (§11, T21).
    async fn list_managed(&self) -> PortResult<Vec<Channel>>;

    /// One page of every channel on the instance, in creation order, for the platform
    /// inventory (§5.4). Unscoped: the caller checks the platform role first.
    async fn list_all(&self, page: PageRequest) -> PortResult<Page<Channel>>;

    /// Every channel id on the instance, ordered, for the federation exchange (§5.3).
    /// Unscoped: never an authorisation path, never hand it to a caller.
    async fn all_ids(&self) -> PortResult<Vec<ChannelId>>;

    /// Every channel's id and policy, for the stale-closure sweep (§7.2bis).
    /// Unscoped, like [`ChannelRepository::all_ids`].
    async fn all_policies(&self) -> PortResult<Vec<(ChannelId, ChannelPolicy)>>;
}
