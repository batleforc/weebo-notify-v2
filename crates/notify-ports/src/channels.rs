//! Channels, their members and their policy.

use notify_domain::{Channel, ChannelId, Principal, Scope};

use crate::error::PortResult;
use crate::pagination::{Page, PageRequest};

/// Channels.
///
/// # Why the reads take groups as well as a principal
///
/// A channel grants access two ways: to a named principal through a
/// [`notify_domain::Membership`], and to everyone in an identity-provider group
/// through a [`notify_domain::GroupBinding`] (design §11). The second has no row
/// per user by design — who is in `infra-admins` is a fact the token carries, and
/// design §5.1 promises we store no session derived from it.
///
/// So every read that answers "which channels may this caller see" needs both, and
/// they are one argument list rather than two calls: a caller that fetched
/// memberships and forgot groups would silently show an empty inbox to everybody
/// whose access is group-based.
#[async_trait::async_trait]
pub trait ChannelRepository: Send + Sync {
    /// Fetch one by name.
    async fn get(&self, id: &ChannelId) -> PortResult<Option<Channel>>;

    /// The channels a caller holds at least `scope` in, by membership or by group.
    ///
    /// This is the query behind almost everything: the inbox, the stream filter,
    /// the search scope. Access is the only door to content (design §5.4), so
    /// getting it from one place means there is one place to get it right.
    async fn list_for(
        &self,
        principal: &Principal,
        groups: &[String],
        scope: Scope,
        page: PageRequest,
    ) -> PortResult<Page<Channel>>;

    /// Every channel id a caller may read, unpaginated.
    ///
    /// Deliberately separate from [`Self::list_for`]: the stream and the sync
    /// backlog need the whole set on every reconnect and never the channel bodies,
    /// and paginating a filter that has to be complete to be correct would just
    /// invite somebody to use the first page of it.
    async fn subscribed_ids(
        &self,
        principal: &Principal,
        groups: &[String],
    ) -> PortResult<Vec<ChannelId>>;

    /// Create or update a channel, members and policy included.
    ///
    /// Used by the API for dynamic channels and by the reconciler for managed ones
    /// (design §11). Refusing to reshape a managed channel is the *use case's* job,
    /// not this port's — the reconciler legitimately needs to write to it.
    async fn upsert(&self, channel: &Channel) -> PortResult<()>;

    /// Delete a channel. Returns whether one was there.
    async fn delete(&self, id: &ChannelId) -> PortResult<bool>;

    /// Every GitOps-managed channel, for reconciliation and drift correction
    /// (design §11, scenario T21).
    async fn list_managed(&self) -> PortResult<Vec<Channel>>;
}
