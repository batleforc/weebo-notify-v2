//! Channels: the unit of membership, policy and fan-out (design §2.1).
//!
//! A channel is the *only* door to content. Being a platform admin gets you the
//! inventory, the quotas and the audit log, but not a single notification body of
//! a channel you are not a member of (design §5.4) — so "who may read this" has
//! exactly one answer everywhere, and it is this type.
//!
//! A channel is [`Origin::Managed`] or [`Origin::Dynamic`] like every other
//! reconciled object (design §11): config owns the shape of the first, the API owns
//! the shape of the second.

use serde::{Deserialize, Serialize};

use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, optional_text};
use crate::ids::ChannelId;
use crate::limits::MAX_TITLE_CHARS;
use crate::notification::Priority;
use crate::origin::Origin;
use crate::principal::Principal;
use crate::ttl::TtlPolicy;

/// What a principal may do in a channel (design §5.2).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Write notifications and revisions.
    Publish,
    /// Read the inbox, the stream and the sync backlog.
    Subscribe,
    /// Administer *this* channel: members, keys, policy. Not the instance —
    /// that is the platform role (design §5.4).
    Manage,
}

/// The scopes one principal holds in one channel.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership {
    /// Who. Users and service accounts are members in exactly the same way
    /// (design §5.2bis).
    pub principal: Principal,
    /// What they may do. Empty means a member with no rights, which is a
    /// configuration mistake rather than a state the domain needs to model — so it
    /// is refused at construction.
    pub scopes: Vec<Scope>,
}

impl Membership {
    /// Build a membership, refusing the rightless case.
    pub fn new(principal: Principal, scopes: Vec<Scope>) -> Result<Self> {
        if scopes.is_empty() {
            return Err(DomainError::Empty {
                field: "member.scopes",
            });
        }
        let mut scopes = scopes;
        scopes.sort_unstable();
        scopes.dedup();
        Ok(Self { principal, scopes })
    }

    /// Whether this membership grants `scope`.
    #[must_use]
    pub fn grants(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }
}

/// The scopes everyone in an identity-provider group holds in one channel
/// (design §11, `[[channels.managed.access]] oidc_group`).
///
/// Deliberately *not* a [`Membership`]: there is no principal to name. Who is in
/// `infra-admins` is a fact the identity provider owns and the token carries, and
/// it changes without anyone telling us. Resolving it at authorisation time is what
/// keeps design §5.1's promise — pure JWKS validation, zero server session — and it
/// is why somebody removed from a group loses access on their next token rather
/// than whenever a reconciler next runs.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupBinding {
    /// The group as the `groups` claim spells it.
    pub group: String,
    /// What its members may do here.
    pub scopes: Vec<Scope>,
}

impl GroupBinding {
    /// Build a binding, refusing the rightless case for the same reason
    /// [`Membership::new`] does.
    pub fn new(group: impl Into<String>, scopes: Vec<Scope>) -> Result<Self> {
        let group = group.into();
        if group.trim().is_empty() {
            return Err(DomainError::Empty {
                field: "access.oidc_group",
            });
        }
        if scopes.is_empty() {
            return Err(DomainError::Empty {
                field: "access.scopes",
            });
        }

        let mut scopes = scopes;
        scopes.sort_unstable();
        scopes.dedup();
        Ok(Self { group, scopes })
    }

    /// Whether this binding grants `scope`.
    #[must_use]
    pub fn grants(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }
}

/// The knobs that apply to everything published in a channel.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelPolicy {
    /// Retention rules (design §7.2bis).
    pub ttl: TtlPolicy,
    /// Priority applied when a publisher says nothing.
    pub default_priority: Priority,
    /// Whether delivery and read receipts are collected and shown as "seen by
    /// 3/5" (design §7.3). Opt-in: a channel that tells publishers who read what
    /// is a product decision, not a default.
    pub receipts_enabled: bool,
}

impl Default for ChannelPolicy {
    fn default() -> Self {
        Self {
            ttl: TtlPolicy::default(),
            default_priority: Priority::Default,
            receipts_enabled: false,
        }
    }
}

/// A channel.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    /// Operator-chosen name, and the id used in URLs.
    pub id: ChannelId,
    /// Human label. Falls back to the id when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Where it came from.
    pub origin: Origin,
    /// Its policy.
    pub policy: ChannelPolicy,
    /// Its members, users and service accounts alike.
    pub members: Vec<Membership>,
    /// Access granted to whole identity-provider groups (design §11).
    ///
    /// Resolved against the `groups` claim of the caller's token rather than stored
    /// per user — see [`GroupBinding`]. Defaulted on the wire so a client written
    /// against an older build still parses a channel.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_access: Vec<GroupBinding>,
}

impl Channel {
    /// A dynamic channel with default policy and no members.
    pub fn dynamic(id: ChannelId, display_name: Option<&str>) -> Result<Self> {
        Ok(Self {
            id,
            display_name: optional_text(
                display_name,
                TextKind::SingleLine,
                MAX_TITLE_CHARS,
                "channel.display_name",
            )?,
            origin: Origin::Dynamic,
            policy: ChannelPolicy::default(),
            members: Vec::new(),
            group_access: Vec::new(),
        })
    }

    /// What a client shows.
    #[must_use]
    pub fn label(&self) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| self.id.to_string())
    }

    /// The membership of `principal`, if any.
    #[must_use]
    pub fn membership(&self, principal: &Principal) -> Option<&Membership> {
        self.members
            .iter()
            .find(|member| &member.principal == principal)
    }

    /// Whether `principal` holds `scope` here.
    ///
    /// Channel keys are deliberately *not* resolved through membership: a key
    /// carries its own scopes and its own channel list (design §5.2), and the
    /// authentication adapter checks those. Mixing the two would give a key rights
    /// nobody granted it.
    #[must_use]
    pub fn grants(&self, principal: &Principal, scope: Scope) -> bool {
        self.membership(principal)
            .is_some_and(|member| member.grants(scope))
    }

    /// Whether any of these groups holds `scope` here (design §11).
    ///
    /// The second door to a channel, and the one that needs no row per user: an
    /// identity provider that stops putting somebody in `infra-admins` revokes
    /// their access on their next token, with nothing to reconcile.
    ///
    /// An empty group list answers `false` for every scope, which is exactly the
    /// degradation an issuer that emits no groups at all requires (scenario T29):
    /// the caller falls back to their individual bindings and reaches nothing else.
    #[must_use]
    pub fn grants_group(&self, groups: &[String], scope: Scope) -> bool {
        self.group_access
            .iter()
            .any(|binding| binding.grants(scope) && groups.contains(&binding.group))
    }

    /// Whether `principal`, holding `groups`, may do `scope` here.
    ///
    /// The two doors in one place, so no caller can check one and forget the other.
    #[must_use]
    pub fn allows(&self, principal: &Principal, groups: &[String], scope: Scope) -> bool {
        self.grants(principal, scope) || self.grants_group(groups, scope)
    }

    /// Add or replace a group binding.
    pub fn upsert_group(&mut self, binding: GroupBinding) {
        match self
            .group_access
            .iter_mut()
            .find(|existing| existing.group == binding.group)
        {
            Some(existing) => *existing = binding,
            None => self.group_access.push(binding),
        }
    }

    /// Add or replace a membership.
    pub fn upsert_member(&mut self, membership: Membership) {
        match self
            .members
            .iter_mut()
            .find(|member| member.principal == membership.principal)
        {
            Some(existing) => *existing = membership,
            None => self.members.push(membership),
        }
    }

    /// Remove a membership. Returns whether anything was removed, so a caller can
    /// tell "left the channel" from "was not in it".
    pub fn remove_member(&mut self, principal: &Principal) -> bool {
        let before = self.members.len();
        self.members.retain(|member| &member.principal != principal);
        self.members.len() != before
    }

    /// Refuse destructive API operations on a GitOps-owned channel: config is the
    /// source of truth and would put it straight back (design §8, §11).
    pub fn ensure_mutable_through_api(&self) -> Result<()> {
        self.origin.ensure_mutable_through_api("channel")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;
    use crate::ids::{ServiceAccountId, UserId};

    fn user() -> Principal {
        Principal::User {
            id: UserId::generate(),
        }
    }

    fn channel() -> Channel {
        Channel::dynamic(ChannelId::parse("ci-infra").unwrap(), None).unwrap()
    }

    #[test]
    fn scopes_gate_what_a_member_may_do() {
        let mut channel = channel();
        let alice = user();
        channel.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());

        assert!(channel.grants(&alice, Scope::Subscribe));
        assert!(!channel.grants(&alice, Scope::Publish));
        assert!(
            !channel.grants(&user(), Scope::Subscribe),
            "a stranger holds nothing"
        );
    }

    #[test]
    fn a_service_account_is_a_member_like_any_other() {
        let mut channel = channel();
        let bot = Principal::ServiceAccount {
            id: ServiceAccountId::parse("bot-argocd").unwrap(),
        };
        channel.upsert_member(Membership::new(bot.clone(), vec![Scope::Publish]).unwrap());

        assert!(channel.grants(&bot, Scope::Publish));
        assert!(!channel.grants(&bot, Scope::Manage));
    }

    #[test]
    fn upserting_a_membership_replaces_it_rather_than_accumulating() {
        let mut channel = channel();
        let alice = user();
        channel.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());
        channel.upsert_member(Membership::new(alice.clone(), vec![Scope::Publish]).unwrap());

        assert_eq!(channel.members.len(), 1);
        assert!(!channel.grants(&alice, Scope::Subscribe));
        assert!(channel.grants(&alice, Scope::Publish));
    }

    #[test]
    fn removing_a_member_reports_whether_it_did_anything() {
        let mut channel = channel();
        let alice = user();
        channel.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());

        assert!(channel.remove_member(&alice));
        assert!(!channel.remove_member(&alice));
    }

    #[test]
    fn a_rightless_membership_is_a_mistake_not_a_state() {
        assert!(Membership::new(user(), vec![]).is_err());
    }

    #[test]
    fn a_managed_channel_refuses_api_mutation() {
        let mut managed = channel();
        managed.origin = Origin::Managed;

        assert!(channel().ensure_mutable_through_api().is_ok());
        assert!(
            managed.ensure_mutable_through_api().is_err(),
            "deleting it through the API would just have GitOps put it back"
        );
    }
}

#[cfg(test)]
mod group_access_tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;
    use crate::ids::UserId;

    fn user() -> Principal {
        Principal::User {
            id: UserId::generate(),
        }
    }

    fn bound(group: &str, scopes: Vec<Scope>) -> Channel {
        let mut channel = Channel::dynamic(ChannelId::parse("ci-infra").unwrap(), None).unwrap();
        channel.upsert_group(GroupBinding::new(group, scopes).unwrap());
        channel
    }

    #[test]
    fn a_group_binding_grants_without_a_membership() {
        // The whole point: nobody wrote a row for this person, and they still get in.
        let channel = bound("infra-admins", vec![Scope::Publish, Scope::Subscribe]);
        let alice = user();
        let groups = vec!["infra-admins".to_owned()];

        assert!(
            !channel.grants(&alice, Scope::Publish),
            "she is not a member"
        );
        assert!(channel.allows(&alice, &groups, Scope::Publish));
        assert!(channel.allows(&alice, &groups, Scope::Subscribe));
        assert!(
            !channel.allows(&alice, &groups, Scope::Manage),
            "a binding grants its scopes and no others"
        );
    }

    #[test]
    fn leaving_a_group_revokes_access_with_nothing_to_reconcile() {
        // The reason the model is derived rather than materialised: the next token
        // simply carries fewer groups.
        let channel = bound("infra-admins", vec![Scope::Subscribe]);
        let alice = user();

        assert!(channel.allows(&alice, &["infra-admins".to_owned()], Scope::Subscribe));
        assert!(!channel.allows(&alice, &["dev-team".to_owned()], Scope::Subscribe));
    }

    #[test]
    fn critical_t29_an_issuer_that_emits_no_groups_degrades_to_individual_bindings() {
        // Dex sends no `groups` claim at all. The required behaviour is that the
        // caller reaches their own bindings and nothing else — never a default grant.
        let mut channel = bound("infra-admins", vec![Scope::Subscribe]);
        let (alice, carol) = (user(), user());
        channel.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());

        assert!(
            channel.allows(&alice, &[], Scope::Subscribe),
            "her individual binding still works"
        );
        assert!(
            !channel.allows(&carol, &[], Scope::Subscribe),
            "and an empty group list is never a default grant"
        );
    }

    #[test]
    fn a_binding_is_replaced_rather_than_stacked() {
        let mut channel = bound("infra-admins", vec![Scope::Subscribe]);
        channel.upsert_group(GroupBinding::new("infra-admins", vec![Scope::Publish]).unwrap());

        assert_eq!(channel.group_access.len(), 1);
        assert!(channel.allows(&user(), &["infra-admins".to_owned()], Scope::Publish));
        assert!(!channel.allows(&user(), &["infra-admins".to_owned()], Scope::Subscribe));
    }

    #[test]
    fn a_rightless_or_nameless_binding_is_refused_at_construction() {
        assert!(GroupBinding::new("infra-admins", Vec::new()).is_err());
        assert!(GroupBinding::new("  ", vec![Scope::Subscribe]).is_err());
        // Duplicates are normalised away, like a membership's.
        let binding = GroupBinding::new(
            "infra-admins",
            vec![Scope::Subscribe, Scope::Subscribe, Scope::Publish],
        )
        .unwrap();
        assert_eq!(binding.scopes, [Scope::Publish, Scope::Subscribe]);
    }

    #[test]
    fn a_channel_without_group_access_keeps_it_off_the_wire() {
        let channel = Channel::dynamic(ChannelId::parse("ci-infra").unwrap(), None).unwrap();
        let json = serde_json::to_string(&channel).unwrap();

        assert!(!json.contains("group_access"));
        // And a payload written before this field existed still parses.
        assert_eq!(serde_json::from_str::<Channel>(&json).unwrap(), channel);
    }
}
