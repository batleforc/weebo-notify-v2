//! Authorisation: the one place that answers "may this principal do that here" (§4).
//! Members are authorised by membership, keys by their own scopes and channels (§5.2).
//! A refusal is always `scope.missing`, never "no such channel", so nothing enumerates.

use notify_domain::{ApiKeyId, Channel, ChannelId, DomainError, Principal, Scope, UserId};
use notify_ports::{ChannelRepository, Failure, PageRequest};
use time::OffsetDateTime;

/// The credential a request arrived with, once authenticated; separate from
/// [`Principal`] because one identity can act with different grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// Rights come from channel membership.
    Membership,
    /// Rights come from an API key: these scopes, in these channels, nothing else (§5.2).
    ApiKey {
        /// Which key, so its publish quota is its own (design §8.2).
        id: ApiKeyId,
        /// What the key may do.
        scopes: Vec<Scope>,
        /// Where it may do it.
        channels: Vec<ChannelId>,
        /// This key's own publish ceiling, if a platform-admin set one (§8.2).
        publish_per_minute: Option<u32>,
        /// For a personal key, its owner's last-login groups; only used to check the owner
        /// still holds what the key grants, never to widen it.
        owner_groups: Vec<String>,
    },
    /// Rights come from a federated CI token, resolved from a trust policy (§5.3).
    Federated {
        /// Each channel with exactly the scopes the policies grant there; kept paired so
        /// channels do not get the union of every policy's scopes.
        grants: Vec<(ChannelId, Vec<Scope>)>,
    },
}

impl Grant {
    /// Whether this grant, on its own, carries `scope` in `channel`; always `false` for
    /// `Membership`, which needs the channel (see [`Authorizer`]).
    #[must_use]
    pub fn carries(&self, channel: &ChannelId, scope: Scope) -> bool {
        match self {
            Self::Membership => false,
            Self::ApiKey {
                scopes, channels, ..
            } => scopes.contains(&scope) && channels.contains(channel),
            Self::Federated { grants } => grants
                .iter()
                .any(|(id, scopes)| id == channel && scopes.contains(&scope)),
        }
    }
}

/// An authenticated caller: who they are, and where their rights come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// The identity a write is attributed to (design §5).
    pub principal: Principal,
    /// Where its rights come from.
    pub grant: Grant,
    /// The groups this request's token carried (§11); never stored, so removal from a
    /// group takes effect on the next token. Empty for keys, service accounts and CI.
    pub groups: Vec<String>,
    /// Whether this request carries the platform role (§5.4): `[admin] oidc_groups`
    /// intersected with the token's groups, never stored. Always `false` for non-OIDC callers.
    pub platform_admin: bool,
    /// When this credential stops being valid (`None`: never). Long-lived connections
    /// close at this instant.
    pub expires_at: Option<OffsetDateTime>,
}

impl Caller {
    /// A caller acting through channel membership alone — a service account through
    /// the UI, or a human whose issuer released no groups.
    #[must_use]
    pub const fn member(principal: Principal) -> Self {
        Self {
            principal,
            grant: Grant::Membership,
            groups: Vec::new(),
            platform_admin: false,
            expires_at: None,
        }
    }

    /// The same, carrying the groups the token released.
    #[must_use]
    pub fn member_of(principal: Principal, groups: Vec<String>) -> Self {
        Self {
            groups,
            ..Self::member(principal)
        }
    }

    /// The user behind a person's own session, or a refusal: a key acts for a user but
    /// must not change the account itself.
    pub const fn ensure_person(&self) -> Result<UserId, DomainError> {
        match (&self.principal, &self.grant) {
            (Principal::User { id }, Grant::Membership) => Ok(*id),
            (Principal::User { .. }, _) => Err(DomainError::SessionRequired),
            _ => Err(DomainError::NotAPerson),
        }
    }

    /// Refuse anything but an interactive session: devices, keys and service accounts are
    /// managed by a person, never by a key or a CI token that could reroute or revoke them.
    pub fn ensure_session(&self) -> Result<(), DomainError> {
        if self.grant == Grant::Membership {
            return Ok(());
        }
        Err(DomainError::SessionRequired)
    }

    /// Refuse anyone without the platform role (§5.4): a 403, not a 404, since the
    /// console is not a secret.
    pub const fn ensure_platform_admin(&self) -> Result<(), DomainError> {
        if self.platform_admin {
            return Ok(());
        }
        Err(DomainError::NotPlatformAdmin)
    }
}

/// Resolves authorisation questions against the channel repository.
#[derive(Clone)]
pub struct Authorizer {
    channels: std::sync::Arc<dyn ChannelRepository>,
}

impl core::fmt::Debug for Authorizer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Authorizer").finish_non_exhaustive()
    }
}

impl Authorizer {
    /// Build one.
    #[must_use]
    pub fn new(channels: std::sync::Arc<dyn ChannelRepository>) -> Self {
        Self { channels }
    }

    /// Fetch a channel the caller holds `scope` in, or refuse; returned so callers act
    /// on exactly what was authorised.
    pub async fn authorize(
        &self,
        caller: &Caller,
        channel: &ChannelId,
        scope: Scope,
    ) -> Result<Channel, Failure> {
        let found = self.channels.get(channel).await?;

        // Missing and forbidden answer the same, so the instance cannot be enumerated.
        let Some(found) = found else {
            return Err(Self::refuse(scope));
        };

        if Self::allows(caller, &found, scope) {
            Ok(found)
        } else {
            Err(Self::refuse(scope))
        }
    }

    /// Whether the caller holds `scope` in `channel`, already fetched.
    fn allows(caller: &Caller, channel: &Channel, scope: Scope) -> bool {
        match &caller.grant {
            // Both doors (§11): named membership, or a group the token carries.
            Grant::Membership => channel.allows(&caller.principal, &caller.groups, scope),
            // A key's rights come from itself; a personal key is also capped by its owner's
            // membership, so it does not outlive their removal.
            grant @ Grant::ApiKey { owner_groups, .. } => {
                grant.carries(&channel.id, scope)
                    && (!matches!(caller.principal, Principal::User { .. })
                        || channel.allows(&caller.principal, owner_groups, scope))
            }
            grant @ Grant::Federated { .. } => grant.carries(&channel.id, scope),
        }
    }

    /// The scopes the caller holds in `channel` (RFC 0004 decision 4), so a client
    /// shows only the controls it may use.
    #[must_use]
    pub fn scopes_in(caller: &Caller, channel: &Channel) -> Vec<Scope> {
        [Scope::Subscribe, Scope::Publish, Scope::Manage]
            .into_iter()
            .filter(|scope| Self::allows(caller, channel, *scope))
            .collect()
    }

    /// Every channel the caller manages through membership, named or by group.
    /// Empty for a key or a CI token: administration is a person's session.
    pub async fn managed_channels(&self, caller: &Caller) -> Result<Vec<ChannelId>, Failure> {
        let mut ids = Vec::new();
        if caller.grant != Grant::Membership {
            return Ok(ids);
        }
        let mut page = PageRequest::of_size(PageRequest::MAX_LIMIT);
        loop {
            let found = self
                .channels
                .list_for(&caller.principal, &caller.groups, Scope::Manage, page)
                .await?;
            ids.extend(found.items.into_iter().map(|channel| channel.id));
            match found.next {
                Some(cursor) => page = page.after(cursor),
                None => return Ok(ids),
            }
        }
    }

    /// Every channel the caller may read, which is what scopes the stream, the sync
    /// backlog and search (scenario T27).
    pub async fn readable_channels(&self, caller: &Caller) -> Result<Vec<ChannelId>, Failure> {
        Ok(match &caller.grant {
            Grant::Membership => {
                self.channels
                    .subscribed_ids(&caller.principal, &caller.groups)
                    .await?
            }
            Grant::ApiKey {
                scopes,
                channels,
                owner_groups,
                ..
            } => {
                if !scopes.contains(&Scope::Subscribe) {
                    // A publish-only key reads nothing: an empty scope, not an error.
                    return Ok(Vec::new());
                }
                if matches!(caller.principal, Principal::User { .. }) {
                    // Same cap as `authorize`: what the owner can still read.
                    let owner = self
                        .channels
                        .subscribed_ids(&caller.principal, owner_groups)
                        .await?;
                    channels
                        .iter()
                        .filter(|channel| owner.contains(channel))
                        .cloned()
                        .collect()
                } else {
                    channels.clone()
                }
            }
            Grant::Federated { grants } => grants
                .iter()
                .filter(|(_, scopes)| scopes.contains(&Scope::Subscribe))
                .map(|(channel, _)| channel.clone())
                .collect(),
        })
    }

    fn refuse(scope: Scope) -> Failure {
        Failure::Domain(DomainError::ScopeMissing {
            scope: match scope {
                Scope::Publish => "publish",
                Scope::Subscribe => "subscribe",
                Scope::Manage => "manage",
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use notify_domain::{Channel, Membership, UserId};
    use notify_ports::test_utils::InMemory;

    use super::*;

    fn channel_id(id: &str) -> ChannelId {
        ChannelId::parse(id).unwrap()
    }

    fn user() -> Principal {
        Principal::User {
            id: UserId::generate(),
        }
    }

    fn store_with(channel: Channel) -> (InMemory, Authorizer) {
        let store = InMemory::new();
        store.seed_channel(channel);
        let authorizer = Authorizer::new(Arc::new(store.clone()));
        (store, authorizer)
    }

    fn channel_for(id: &str, member: &Principal, scopes: Vec<Scope>) -> Channel {
        let mut channel = Channel::dynamic(channel_id(id), None).unwrap();
        channel.upsert_member(Membership::new(member.clone(), scopes).unwrap());
        channel
    }

    #[tokio::test]
    async fn a_member_is_authorised_by_their_scopes() {
        let alice = user();
        let (_store, authorizer) =
            store_with(channel_for("ci-infra", &alice, vec![Scope::Subscribe]));
        let caller = Caller::member(alice);

        assert!(
            authorizer
                .authorize(&caller, &channel_id("ci-infra"), Scope::Subscribe)
                .await
                .is_ok()
        );
        assert_eq!(
            authorizer
                .authorize(&caller, &channel_id("ci-infra"), Scope::Publish)
                .await
                .unwrap_err()
                .code(),
            Some("scope.missing")
        );
    }

    #[tokio::test]
    async fn a_missing_channel_is_indistinguishable_from_one_you_may_not_touch() {
        let alice = user();
        let (_store, authorizer) =
            store_with(channel_for("ci-infra", &alice, vec![Scope::Subscribe]));
        let caller = Caller::member(alice);

        let forbidden = authorizer
            .authorize(&caller, &channel_id("ci-infra"), Scope::Manage)
            .await
            .unwrap_err();
        let absent = authorizer
            .authorize(&caller, &channel_id("does-not-exist"), Scope::Subscribe)
            .await
            .unwrap_err();

        assert_eq!(
            forbidden.code(),
            absent.code(),
            "distinguishing the two would let a caller map the instance"
        );
    }

    // --- T12 · A key's rights come from the key (design §3.4) --------------

    #[tokio::test]
    async fn critical_t12_a_key_is_not_authorised_through_its_owners_membership() {
        let alice = user();
        // Alice can do everything in the channel…
        let (_store, authorizer) = store_with(channel_for(
            "ci-infra",
            &alice,
            vec![Scope::Publish, Scope::Subscribe, Scope::Manage],
        ));

        // …but her publish-only key can only publish.
        let caller = Caller {
            principal: alice,
            grant: Grant::ApiKey {
                id: ApiKeyId::generate(),
                scopes: vec![Scope::Publish],
                channels: vec![channel_id("ci-infra")],
                publish_per_minute: None,
                owner_groups: Vec::new(),
            },
            groups: Vec::new(),
            platform_admin: false,
            expires_at: None,
        };

        assert!(
            authorizer
                .authorize(&caller, &channel_id("ci-infra"), Scope::Publish)
                .await
                .is_ok()
        );
        for denied in [Scope::Subscribe, Scope::Manage] {
            assert!(
                authorizer
                    .authorize(&caller, &channel_id("ci-infra"), denied)
                    .await
                    .is_err(),
                "a key must not inherit what its owner happens to hold"
            );
        }
    }

    #[tokio::test]
    async fn critical_t12_a_key_grants_nothing_outside_its_channel_list() {
        let alice = user();
        let (store, authorizer) = store_with(channel_for(
            "ci-infra",
            &alice,
            vec![Scope::Publish, Scope::Subscribe],
        ));
        store.seed_channel(channel_for(
            "cluster-ovh-1",
            &alice,
            vec![Scope::Publish, Scope::Subscribe],
        ));

        let caller = Caller {
            principal: alice,
            grant: Grant::ApiKey {
                id: ApiKeyId::generate(),
                scopes: vec![Scope::Publish],
                channels: vec![channel_id("ci-infra")],
                publish_per_minute: None,
                owner_groups: Vec::new(),
            },
            groups: Vec::new(),
            platform_admin: false,
            expires_at: None,
        };
        assert!(
            authorizer
                .authorize(&caller, &channel_id("cluster-ovh-1"), Scope::Publish)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_personal_key_stops_working_once_its_owner_leaves_the_channel() {
        let alice = user();
        // Alice is no longer a member of ci-infra; her key still names it.
        let (_store, authorizer) =
            store_with(channel_for("ci-infra", &user(), vec![Scope::Subscribe]));
        let key = |owner_groups: Vec<String>| Caller {
            principal: alice.clone(),
            grant: Grant::ApiKey {
                id: ApiKeyId::generate(),
                scopes: vec![Scope::Subscribe],
                channels: vec![channel_id("ci-infra")],
                publish_per_minute: None,
                owner_groups,
            },
            groups: Vec::new(),
            platform_admin: false,
            expires_at: None,
        };

        assert!(
            authorizer
                .authorize(&key(Vec::new()), &channel_id("ci-infra"), Scope::Subscribe)
                .await
                .is_err()
        );
        assert!(
            authorizer
                .readable_channels(&key(Vec::new()))
                .await
                .unwrap()
                .is_empty(),
            "removal from the channel must reach the key, not wait for its expiry"
        );
    }

    #[tokio::test]
    async fn a_personal_key_reaches_what_its_owners_remembered_groups_reach() {
        let alice = user();
        let mut channel = Channel::dynamic(channel_id("ci-infra"), None).unwrap();
        channel.upsert_group(
            notify_domain::GroupBinding::new("infra-admins", vec![Scope::Subscribe]).unwrap(),
        );
        let (_store, authorizer) = store_with(channel);
        let caller = Caller {
            principal: alice,
            grant: Grant::ApiKey {
                id: ApiKeyId::generate(),
                scopes: vec![Scope::Subscribe],
                channels: vec![channel_id("ci-infra")],
                publish_per_minute: None,
                owner_groups: vec!["infra-admins".to_owned()],
            },
            groups: Vec::new(),
            platform_admin: false,
            expires_at: None,
        };

        assert!(
            authorizer
                .authorize(&caller, &channel_id("ci-infra"), Scope::Subscribe)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_publish_only_key_reads_no_channels_at_all() {
        let alice = user();
        let (_store, authorizer) = store_with(channel_for(
            "ci-infra",
            &alice,
            vec![Scope::Publish, Scope::Subscribe],
        ));

        let publish_only = Caller {
            principal: alice.clone(),
            grant: Grant::ApiKey {
                id: ApiKeyId::generate(),
                scopes: vec![Scope::Publish],
                channels: vec![channel_id("ci-infra")],
                publish_per_minute: None,
                owner_groups: Vec::new(),
            },
            groups: Vec::new(),
            platform_admin: false,
            expires_at: None,
        };
        assert_eq!(
            authorizer.readable_channels(&publish_only).await.unwrap(),
            [] as [notify_domain::ChannelId; 0]
        );

        // The same person through the UI reads what their membership says.
        assert_eq!(
            authorizer
                .readable_channels(&Caller::member(alice))
                .await
                .unwrap(),
            vec![channel_id("ci-infra")]
        );
    }
}
