//! Authorisation: the one place that answers "may this principal do that here".
//!
//! Ports answer "who could" and never "who may" (design §4), so every membership
//! and scope check funnels through this module. That is what makes "membership is
//! the only door to content" (design §5.4) a property of the system rather than a
//! habit each handler has to remember.
//!
//! # Two doors, deliberately not merged
//!
//! A **user or service account** is authorised through channel membership. An
//! **API key** carries its own scopes and its own channel list and is authorised
//! against those (design §5.2) — resolving a key through membership would hand it
//! rights nobody granted it, and resolving a member through key scopes would break
//! the moment they have no key.
//!
//! # What a refusal says
//!
//! Always `scope.missing`, never "no such channel". Telling somebody a channel
//! exists but is not theirs is an enumeration oracle: a caller who can distinguish
//! the two can map the whole instance. The log records which it was; the caller
//! does not.

use notify_domain::{Channel, ChannelId, DomainError, Principal, Scope};
use notify_ports::{ChannelRepository, Failure};

/// The credential a request arrived with, once authenticated.
///
/// Kept separate from [`Principal`] because the *identity* and the *grant* are
/// different things: the same service account acts with full membership through the
/// UI and with a publish-only key from a pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// Rights come from channel membership.
    Membership,
    /// Rights come from an API key: these scopes, in these channels, and nothing
    /// else (design §5.2).
    ApiKey {
        /// What the key may do.
        scopes: Vec<Scope>,
        /// Where it may do it.
        channels: Vec<ChannelId>,
    },
    /// Rights come from a federated CI token, resolved from a trust policy
    /// (design §5.3). Same shape as a key, different door.
    Federated {
        /// What the token may do.
        scopes: Vec<Scope>,
        /// Where it may do it.
        channels: Vec<ChannelId>,
    },
}

impl Grant {
    /// Whether this grant, on its own, carries `scope` in `channel`.
    ///
    /// `Membership` returns `false` here: it needs the channel to answer, which is
    /// what [`Authorizer`] is for.
    #[must_use]
    pub fn carries(&self, channel: &ChannelId, scope: Scope) -> bool {
        match self {
            Self::Membership => false,
            Self::ApiKey { scopes, channels } | Self::Federated { scopes, channels } => {
                scopes.contains(&scope) && channels.contains(channel)
            }
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
    /// The groups this request's token carried (design §11).
    ///
    /// Held on the *caller* rather than looked up, because that is the only place
    /// they exist: group membership is a claim the identity provider signs, and
    /// design §5.1 stores no session derived from it. A caller removed from a group
    /// therefore loses that access on their next token, with nothing to reconcile.
    ///
    /// Empty for an API key, a service account and any issuer that emits no groups
    /// at all — the last is scenario T29, and the required behaviour is that they
    /// reach their individual bindings and nothing else.
    pub groups: Vec<String>,
    /// Whether this request carries the instance-wide platform role (design §5.4).
    ///
    /// **Derived, never stored**, on exactly the same terms as `groups` above and
    /// for the same reason (§14.2): it is `[admin] oidc_groups` intersected with
    /// what this token says. Somebody removed from the admin group loses the role on
    /// their next token, with nothing to reconcile and no row to forget.
    ///
    /// Always `false` for an API key, a service account and a CI job. §5.4 reserves
    /// it to OIDC users: a bot scoped to its channels does not administer the
    /// platform, and no API path grants this — it comes from GitOps or not at all.
    pub platform_admin: bool,
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

    /// Refuse anyone without the platform role (design §5.4, scenario T30).
    ///
    /// # Errors
    ///
    /// [`DomainError::NotPlatformAdmin`] — a `403`. Not a `404`: the console is not
    /// a secret, and hiding its existence from an authenticated user buys nothing
    /// while making "why does this button do nothing" unanswerable.
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

    /// Fetch a channel the caller holds `scope` in, or refuse.
    ///
    /// Returns the channel because almost every caller needs it next — its policy,
    /// its members — and a second round trip to fetch what we just authorised
    /// against would be a second chance to authorise against something different.
    pub async fn authorize(
        &self,
        caller: &Caller,
        channel: &ChannelId,
        scope: Scope,
    ) -> Result<Channel, Failure> {
        let found = self.channels.get(channel).await?;

        // A channel that does not exist and one the caller may not touch produce
        // the same answer: distinguishing them enumerates the instance.
        let Some(found) = found else {
            return Err(Self::refuse(scope));
        };

        let allowed = match &caller.grant {
            // Both doors in one call (design §11): named membership, or a group the
            // token carries. Checking one and forgetting the other is exactly how a
            // group-based instance shows everybody an empty inbox.
            Grant::Membership => found.allows(&caller.principal, &caller.groups, scope),
            // A key still has to name a real channel, but its rights come from
            // itself — not from whatever membership its owner happens to have.
            grant => grant.carries(channel, scope),
        };

        if allowed {
            Ok(found)
        } else {
            Err(Self::refuse(scope))
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
            Grant::ApiKey { scopes, channels } | Grant::Federated { scopes, channels } => {
                if scopes.contains(&Scope::Subscribe) {
                    channels.clone()
                } else {
                    // A publish-only key reads nothing, and gets an empty scope
                    // rather than an error: listing zero channels is a valid answer.
                    Vec::new()
                }
            }
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
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

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
                scopes: vec![Scope::Publish],
                channels: vec![channel_id("ci-infra")],
            },
            groups: Vec::new(),
            platform_admin: false,
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
                scopes: vec![Scope::Publish],
                channels: vec![channel_id("ci-infra")],
            },
            groups: Vec::new(),
            platform_admin: false,
        };
        assert!(
            authorizer
                .authorize(&caller, &channel_id("cluster-ovh-1"), Scope::Publish)
                .await
                .is_err()
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
                scopes: vec![Scope::Publish],
                channels: vec![channel_id("ci-infra")],
            },
            groups: Vec::new(),
            platform_admin: false,
        };
        assert!(
            authorizer
                .readable_channels(&publish_only)
                .await
                .unwrap()
                .is_empty()
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
