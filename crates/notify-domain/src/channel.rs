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

/// The knobs that apply to everything published in a channel.
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
