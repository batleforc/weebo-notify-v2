//! The audit log: what was administered, by whom (design §13).
//!
//! Append-only, and a **separate stream from the technical logs** — this is the one
//! replayed after an incident, and it is no use if it is interleaved with a
//! million request lines. Mirrored to structured logs with `audit=true` so it
//! survives the database it is stored in.
//!
//! What goes in: API key creation and revocation, device and service account
//! lifecycle, membership changes — including a platform admin adding themselves to
//! a channel, which is explicitly allowed and explicitly traced (design §5.4) —
//! trust policy changes, and DLQ requeues.
//!
//! What does not: reads, publishes, receipts. Those are notifications doing their
//! job; recording them would drown the entries that matter in the ones that do not.

use notify_domain::{ChannelId, Principal};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::PortResult;
use crate::pagination::{Page, PageRequest};

/// Who performed an audited action.
///
/// # Why this is not just a [`Principal`]
///
/// §5 has four kinds of principal and every one of them authenticated: a person, a
/// bot, a channel key, a CI job. The reconciler is none of them. It applies a file
/// (§11), at boot or on `SIGHUP`, and nobody "did" that — which is exactly why §13's
/// promise that trust policy changes are journalled had no emitter for so long
/// (D28): there was no honest value to put in this field.
///
/// Widening it here rather than adding a fifth `Principal` is deliberate. A
/// `Principal` is what publishes, holds a membership and reads a channel; a fifth
/// variant would be representable in all of those, and every match over the four
/// would gain an arm that means "cannot happen". The configuration is an actor of
/// the **audit log** and of nothing else, so it is named here and nowhere else.
///
/// The wire form stays flat — `{"kind":"user","id":…}` beside `{"kind":
/// "configuration"}` — so a reader that already switches on `kind` gains a case
/// rather than a nesting level.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AuditActor {
    /// The configuration file, applied by the reconciler.
    ///
    /// **First on purpose.** `untagged` tries the variants in order, and the two are
    /// disjoint by their `kind`: `Principal` refuses `"configuration"` as an unknown
    /// variant and [`Configured`] refuses everything else. Order still matters to a
    /// reader, so it is written down rather than discovered.
    Configuration(Configured),

    /// Somebody or something that authenticated (design §5).
    Principal(Principal),
}

/// The single-valued tag behind [`AuditActor::Configuration`].
///
/// A one-variant enum rather than a unit struct so that serde and utoipa both emit
/// the `{"kind":"configuration"}` object the flat wire form needs.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Configured {
    /// Applied from `[[trust_policy]]`, `[[channels.managed]]` or
    /// `[[service_accounts]]` — GitOps, not a request (design §11).
    Configuration,
}

impl AuditActor {
    /// The reconciler, as the log names it.
    #[must_use]
    pub const fn configuration() -> Self {
        Self::Configuration(Configured::Configuration)
    }

    /// The stable string the log is indexed and filtered on.
    ///
    /// `configuration` cannot collide with a principal's reference: a `UserId` and
    /// the rest are prefixed ULIDs, and a CI subject always contains a colon.
    #[must_use]
    pub fn reference(&self) -> String {
        match self {
            Self::Configuration(_) => "configuration".to_owned(),
            Self::Principal(principal) => principal.reference(),
        }
    }
}

impl From<Principal> for AuditActor {
    fn from(principal: Principal) -> Self {
        Self::Principal(principal)
    }
}

/// One administrative action, from a closed vocabulary (design §13).
///
/// # Why this is a type and not a `&'static str`
///
/// It *was* a string, with the literals written at each call site and the closed
/// list an adapter checks them against living separately, inside that adapter. The
/// two drifted in **both** directions and nothing failed until a row came back:
/// an action nothing emitted sat in the list, while two actions the build did emit
/// were missing from it. Because the check runs on **read**, a single
/// `PATCH /channels/{id}` made `GET /admin/audit` fail for the whole page rather
/// than for the one row — the log design §13 calls "celui qu'on rejoue après un
/// incident" broke on an ordinary edit.
///
/// The field being a string is what made that reachable, so it is not a string any
/// more. The only ways to obtain one are the constants in [`actions`] and
/// [`AuditAction::parse`], which accepts exactly what the constants define. An
/// emitter cannot invent a value, and a value that exists can always be read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AuditAction(&'static str);

impl AuditAction {
    /// The wire form — what is stored, logged and filtered on.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }

    /// Read one back, refusing anything outside the vocabulary.
    ///
    /// The single place that decision is made. An adapter hydrating a stored row
    /// calls this rather than keeping its own list, which is the duplication that
    /// caused the failure above.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        AUDIT_ACTIONS.iter().copied().find(|known| known.0 == value)
    }
}

impl core::fmt::Display for AuditAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

impl PartialEq<str> for AuditAction {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl Serialize for AuditAction {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for AuditAction {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`{value}` is not in the audit vocabulary of this build"
            ))
        })
    }
}

/// Every administrative action this build records (design §13).
pub mod actions {
    use super::AuditAction;

    /// An API key was minted.
    pub const APIKEY_CREATED: AuditAction = AuditAction("apikey.created");
    /// An API key was revoked, which takes effect immediately.
    pub const APIKEY_REVOKED: AuditAction = AuditAction("apikey.revoked");
    /// A platform-admin set (or cleared) a key's own publish ceiling (§8.2, D24).
    pub const APIKEY_LIMIT_SET: AuditAction = AuditAction("apikey.limit_set");
    /// A channel was created through the API.
    pub const CHANNEL_CREATED: AuditAction = AuditAction("channel.created");
    /// A channel was deleted through the API.
    pub const CHANNEL_DELETED: AuditAction = AuditAction("channel.deleted");
    /// Somebody was added to a channel, or their scopes changed.
    pub const CHANNEL_MEMBER_ADDED: AuditAction = AuditAction("channel.member_added");
    /// Somebody took *themselves* out of a channel — no `manage` required, which
    /// is why it is a different action from being removed.
    pub const CHANNEL_MEMBER_LEFT: AuditAction = AuditAction("channel.member_left");
    /// Somebody was taken out of a channel by a manager.
    pub const CHANNEL_MEMBER_REMOVED: AuditAction = AuditAction("channel.member_removed");
    /// A channel's retention or receipt policy was edited (`PATCH /channels/{id}`).
    pub const CHANNEL_POLICY_UPDATED: AuditAction = AuditAction("channel.policy_updated");
    /// A device was enrolled.
    pub const DEVICE_ENROLLED: AuditAction = AuditAction("device.enrolled");
    /// A device was revoked, taking its subscription, cursor and key with it.
    pub const DEVICE_REVOKED: AuditAction = AuditAction("device.revoked");
    /// A dead letter was put back on the queue (design §7.1bis).
    pub const DLQ_REQUEUED: AuditAction = AuditAction("dlq.requeued");
    /// The platform role added itself to a channel (design §5.4, scenario T30).
    /// Membership stays the one door to content; this is the record it was used.
    pub const PLATFORM_CHANNEL_SELF_JOIN: AuditAction = AuditAction("platform.channel.self_join");
    /// A service account was created through the API.
    pub const SERVICE_ACCOUNT_CREATED: AuditAction = AuditAction("service_account.created");
    /// A service account was disabled, taking every key it owns with it.
    pub const SERVICE_ACCOUNT_DISABLED: AuditAction = AuditAction("service_account.disabled");

    // --- What GitOps changed (design §11, §13, D28) -------------------------
    //
    // One action per *change*, never one per pass. A row saying "a reconciliation
    // ran" tells an operator that something was applied, which leaves them back at
    // `git log` — the exact gap §13 promises this log closes. Absent changes emit
    // nothing, so a steady instance reconciling every SIGHUP stays silent.

    /// A `[[trust_policy]]` appeared — a forge may now publish here (§5.3).
    pub const TRUST_POLICY_ADDED: AuditAction = AuditAction("trust_policy.added");
    /// A `[[trust_policy]]` was edited: its issuer, channels or subjects moved.
    pub const TRUST_POLICY_CHANGED: AuditAction = AuditAction("trust_policy.changed");
    /// A `[[trust_policy]]` was withdrawn — that forge can no longer publish.
    pub const TRUST_POLICY_REMOVED: AuditAction = AuditAction("trust_policy.removed");

    /// A `[[channels.managed]]` block created a channel.
    pub const MANAGED_CHANNEL_ADDED: AuditAction = AuditAction("managed.channel.added");
    /// A managed channel's declaration changed — access, TTL, limits.
    pub const MANAGED_CHANNEL_CHANGED: AuditAction = AuditAction("managed.channel.changed");
    /// A managed channel left the configuration and was archived (never deleted).
    pub const MANAGED_CHANNEL_REMOVED: AuditAction = AuditAction("managed.channel.removed");

    /// A `[[service_accounts]]` block created an account.
    pub const MANAGED_ACCOUNT_ADDED: AuditAction = AuditAction("managed.service_account.added");
    /// A managed service account's declaration changed.
    pub const MANAGED_ACCOUNT_CHANGED: AuditAction = AuditAction("managed.service_account.changed");
}

/// The closed vocabulary: every value [`AuditAction::parse`] accepts.
///
/// A constant missing from here is one that can be emitted and never read back,
/// which is exactly the failure that made this a type. The test below walks it.
pub const AUDIT_ACTIONS: &[AuditAction] = &[
    actions::APIKEY_CREATED,
    actions::APIKEY_REVOKED,
    actions::APIKEY_LIMIT_SET,
    actions::CHANNEL_CREATED,
    actions::CHANNEL_DELETED,
    actions::CHANNEL_MEMBER_ADDED,
    actions::CHANNEL_MEMBER_LEFT,
    actions::CHANNEL_MEMBER_REMOVED,
    actions::CHANNEL_POLICY_UPDATED,
    actions::DEVICE_ENROLLED,
    actions::DEVICE_REVOKED,
    actions::DLQ_REQUEUED,
    actions::PLATFORM_CHANNEL_SELF_JOIN,
    actions::SERVICE_ACCOUNT_CREATED,
    actions::SERVICE_ACCOUNT_DISABLED,
    actions::TRUST_POLICY_ADDED,
    actions::TRUST_POLICY_CHANGED,
    actions::TRUST_POLICY_REMOVED,
    actions::MANAGED_CHANNEL_ADDED,
    actions::MANAGED_CHANNEL_CHANGED,
    actions::MANAGED_CHANNEL_REMOVED,
    actions::MANAGED_ACCOUNT_ADDED,
    actions::MANAGED_ACCOUNT_CHANGED,
];

#[cfg(test)]
mod action_tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    #[test]
    fn every_action_parses_back_to_itself() {
        // The round trip that the old arrangement could not make: a value that
        // exists must be one the reader accepts. `parse` and the constants read the
        // same list, so this holds by construction — the test is here to fail loudly
        // if somebody ever gives `parse` a list of its own again.
        for action in AUDIT_ACTIONS {
            assert_eq!(
                AuditAction::parse(action.as_str()),
                Some(*action),
                "{action} does not survive a round trip"
            );
        }
    }

    #[test]
    fn an_action_outside_the_vocabulary_is_refused() {
        assert_eq!(AuditAction::parse("apikey.exfiltrated"), None);
    }

    #[test]
    fn the_vocabulary_has_no_duplicates() {
        let mut seen: Vec<&str> = AUDIT_ACTIONS.iter().map(|a| a.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();

        assert_eq!(seen.len(), AUDIT_ACTIONS.len());
    }

    #[test]
    fn an_action_serialises_as_its_wire_string_and_reads_back() {
        // Stored rows and the `GET /admin/audit` body are both this, so a change of
        // shape here is a change of contract.
        let json = serde_json::to_string(&actions::DLQ_REQUEUED).unwrap();
        assert_eq!(json, "\"dlq.requeued\"");
        assert_eq!(
            serde_json::from_str::<AuditAction>(&json).unwrap(),
            actions::DLQ_REQUEUED
        );
        assert!(serde_json::from_str::<AuditAction>("\"nope\"").is_err());
    }
}

/// One administrative act.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Who did it — or [`AuditActor::configuration`] where a file did (§11, D28).
    pub actor: AuditActor,
    /// What they did — one of the constants in [`actions`].
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub action: AuditAction,
    /// What it was done to, e.g. an id. Absent when the action has no single
    /// target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Which channel it concerned, when it concerned one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelId>,
    /// Extra context: the scopes granted, the previous value.
    ///
    /// Never secrets, never payloads. An audit log that quotes an API key is worse
    /// than no audit log, because it is a credential store nobody thinks of as one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// When.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// Which entries to show (design §8, `GET /admin/audit`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditQuery {
    /// Restrict to one actor. Takes the configuration too, which is how an operator
    /// asks "what did GitOps change" without reading every other row.
    pub actor: Option<AuditActor>,
    /// Restrict to one action.
    pub action: Option<String>,
    /// Restrict to one channel.
    pub channel: Option<ChannelId>,
    /// Not before.
    pub since: Option<OffsetDateTime>,
    /// Not after.
    pub until: Option<OffsetDateTime>,
    /// Where and how much.
    pub page: PageRequest,
}

/// The audit log.
#[async_trait::async_trait]
pub trait AuditLog: Send + Sync {
    /// Record an act.
    ///
    /// Called inside the same transaction as the change it describes wherever the
    /// adapter can manage it. An audit log that can disagree with what happened is
    /// not an audit log.
    async fn record(&self, entry: &AuditEntry) -> PortResult<()>;

    /// Browse the log — platform role only, enforced by the use case (design §5.4).
    async fn query(&self, query: &AuditQuery) -> PortResult<Page<AuditEntry>>;
}
