//! CI/CD federation: trading a forge's ID token for a short-lived grant
//! (design §5.3).
//!
//! The point of the whole mechanism is that **no long-lived secret lives in CI**.
//! A job already holds an OIDC token minted by its forge, saying who it is —
//! `repo:weebo/infra:ref:refs/heads/main` — and an operator declares in GitOps
//! which of those subjects may publish where. Nothing is stored on the CI side, so
//! nothing can leak from it.
//!
//! # What this module is, and what it is not
//!
//! It is the *decision*: given claims somebody else vouched for, and the policies
//! this instance was configured with, what may this job do? Verifying the token's
//! signature against the issuer's JWKS is an adapter's job (the `FederationVerifier`
//! port), and minting the resulting access token is the application's. Keeping the
//! three apart is what lets every rule below be tested without a network.
//!
//! # The three hardening rules, and why each exists
//!
//! Scenario T11 (design §3.4) names them, and each is a way the obvious
//! implementation is wrong:
//!
//! * **the audience is checked** — a token minted for another service, by an issuer
//!   we do trust, for a subject we do trust, is still not a token for us. Without
//!   this, any service sharing the forge becomes a way in;
//! * **subject globs are anchored** — `repo:weebo/*` must not match
//!   `repo:weebo-evil/x`. A `starts_with` on the prefix before the `*` does exactly
//!   that, which is why matching happens against the whole string;
//! * **a token is exchanged once** — the `jti` is remembered until the token's own
//!   expiry, so a leaked ID token is worth one exchange rather than fifteen minutes
//!   of them. That store lives behind a port; what is here is the rule that the
//!   `jti` must exist at all.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::channel::Scope;
use crate::error::{DomainError, Result};
use crate::ids::ChannelId;

/// The claims an adapter has verified against an issuer's JWKS.
///
/// Everything here has already been proven to come from the issuer; nothing here
/// has been proven to be *allowed*, which is what [`TrustPolicy`] decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederatedClaims {
    /// Who vouched for it. Compared to a policy's issuer as an exact string: an
    /// issuer is an identity, not a pattern.
    pub issuer: String,
    /// What the job says it is — `repo:weebo/infra:ref:refs/heads/main`.
    pub subject: String,
    /// Who the token was minted for. Must name this instance.
    pub audiences: Vec<String>,
    /// The token's own id, remembered until `expires_at` so it cannot be replayed.
    pub jti: Option<String>,
    /// When the incoming token stops being valid. Bounds how long the `jti` has to
    /// be remembered: past it, a replay would fail on expiry anyway.
    pub expires_at: OffsetDateTime,
}

/// One `[[trust_policy]]` block, or one `[[channels.managed.publishers]]` block
/// (design §11).
///
/// The two are the same thing at different scopes, which is why they are one type:
/// a channel-scoped block is a policy whose channel list is that one channel, and
/// §11 makes it win over a global one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustPolicy {
    /// The issuer this policy trusts. Exact match.
    pub issuer: String,
    /// The subject pattern, matched **anchored** — see [`Pattern`].
    pub subject: Pattern,
    /// Which channels it grants, by pattern. `ci-*` is a policy for every channel
    /// whose name starts with `ci-`, and nothing else.
    pub channels: Vec<Pattern>,
    /// What it grants there.
    pub scopes: Vec<Scope>,
    /// Whether this policy came from a channel's own `publishers` block.
    ///
    /// Carried rather than inferred: §11 makes channel-scoped policies win, and a
    /// decision that reads "the narrower one wins" would be a different rule that
    /// happens to agree most of the time.
    pub channel_scoped: bool,
}

/// A glob matched against a whole string.
///
/// Deliberately tiny — one `*`, meaning "any run of characters including none" —
/// and deliberately **anchored at both ends**. A general glob library would bring
/// `?`, character classes and `**`, none of which any configuration here has asked
/// for, and each of which is another way to write a pattern that matches more than
/// its author believed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Pattern {
    raw: String,
}

impl Pattern {
    /// Build one, refusing what cannot be matched safely.
    ///
    /// # Errors
    ///
    /// An empty pattern, or a bare `*`: both would grant everything the surrounding
    /// policy can express, and an operator who means that should have to be unable
    /// to have meant something else. Neither has a legitimate use here — a policy
    /// trusting every subject of an issuer is a policy trusting whoever can open a
    /// pull request against it.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(DomainError::Empty {
                field: "trust_policy.pattern",
            });
        }
        if raw == "*" {
            return Err(DomainError::Malformed {
                field: "trust_policy.pattern",
                reason: "a bare `*` trusts every subject of that issuer; name what you mean",
            });
        }
        if raw.matches('*').count() > 1 {
            return Err(DomainError::Malformed {
                field: "trust_policy.pattern",
                reason: "at most one `*` is supported",
            });
        }
        Ok(Self {
            raw: raw.to_owned(),
        })
    }

    /// Whether `candidate` matches, **as a whole**.
    ///
    /// This is the T11 clause: `repo:weebo/*` matches `repo:weebo/infra` and
    /// `repo:weebo/` but never `repo:weebo-evil/x`, because the prefix comparison
    /// is followed by a suffix comparison rather than by nothing. The two halves are
    /// also checked not to overlap, so `ab*ba` does not match `aba` twice over.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        match self.raw.split_once('*') {
            None => self.raw == candidate,
            Some((prefix, suffix)) => {
                candidate.len() >= prefix.len() + suffix.len()
                    && candidate.starts_with(prefix)
                    && candidate.ends_with(suffix)
            }
        }
    }

    /// The pattern as written, for the audit log and the admin console.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl TryFrom<String> for Pattern {
    type Error = DomainError;

    fn try_from(raw: String) -> Result<Self> {
        Self::parse(&raw)
    }
}

impl From<Pattern> for String {
    fn from(pattern: Pattern) -> Self {
        pattern.raw
    }
}

impl core::fmt::Display for Pattern {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.raw)
    }
}

/// What an exchange yields: the channels and scopes a job may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederatedGrant {
    /// The issuer that vouched for the job, kept so the minted token and the audit
    /// log can say where the authority came from.
    pub issuer: String,
    /// The subject, which becomes the `CiJob` principal (design §5.1).
    pub subject: String,
    /// Channel patterns the job may act on, and with which scopes.
    pub allowances: Vec<Allowance>,
}

/// One channel pattern and the scopes granted on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowance {
    /// Which channels.
    pub channels: Vec<Pattern>,
    /// What may be done there.
    pub scopes: Vec<Scope>,
}

impl FederatedGrant {
    /// Whether this grant covers `scope` on `channel`.
    #[must_use]
    pub fn allows(&self, channel: &ChannelId, scope: Scope) -> bool {
        self.allowances.iter().any(|allowance| {
            allowance.scopes.contains(&scope)
                && allowance
                    .channels
                    .iter()
                    .any(|pattern| pattern.matches(channel.as_str()))
        })
    }
}

/// The set of policies this instance was configured with (design §11).
#[derive(Debug, Clone, Default)]
pub struct TrustPolicies {
    policies: Vec<TrustPolicy>,
}

impl TrustPolicies {
    /// Build from configuration.
    #[must_use]
    pub fn new(policies: Vec<TrustPolicy>) -> Self {
        Self { policies }
    }

    /// What was configured, for the platform console to display (design §10.4bis).
    ///
    /// Read-only by construction, and that is the whole story a console can tell
    /// about these: they come from GitOps (§11) and no API path edits them. A screen
    /// showing them is answering "what did we deploy", which is exactly the question
    /// somebody has after a pipeline is refused.
    #[must_use]
    pub fn configured(&self) -> &[TrustPolicy] {
        &self.policies
    }

    /// Decide what an authenticated set of claims may do.
    ///
    /// # Errors
    ///
    /// Refuses, in this order and deliberately:
    ///
    /// 1. **a token not minted for us** — before any policy is consulted, so that a
    ///    misdirected token is never answered with "no matching policy", which would
    ///    invite the operator to write one;
    /// 2. **a token with no `jti`** — it cannot be remembered, so it cannot be
    ///    prevented from being replayed, and accepting it would make the anti-replay
    ///    rule opt-out by omission;
    /// 3. **no policy matching** — issuer *and* anchored subject.
    ///
    /// Channel-scoped policies (§11) are collected ahead of global ones, and when
    /// any channel-scoped policy matches, the global ones are **dropped**: a
    /// channel that names its publishers has said who they are, and a broad global
    /// policy must not quietly widen it.
    pub fn decide(&self, claims: &FederatedClaims, audience: &str) -> Result<FederatedGrant> {
        if !claims.audiences.iter().any(|one| one == audience) {
            return Err(DomainError::Malformed {
                field: "token.aud",
                reason: "this token was not minted for weebo-notify",
            });
        }
        if claims.jti.as_deref().unwrap_or_default().trim().is_empty() {
            return Err(DomainError::Malformed {
                field: "token.jti",
                reason: "a token with no id cannot be prevented from being replayed",
            });
        }

        let matching: Vec<&TrustPolicy> = self
            .policies
            .iter()
            .filter(|policy| {
                policy.issuer == claims.issuer && policy.subject.matches(&claims.subject)
            })
            .collect();

        let scoped: Vec<&&TrustPolicy> = matching
            .iter()
            .filter(|policy| policy.channel_scoped)
            .collect();
        let winning: Vec<&TrustPolicy> = if scoped.is_empty() {
            matching.clone()
        } else {
            scoped.into_iter().copied().collect()
        };

        if winning.is_empty() {
            return Err(DomainError::Malformed {
                field: "trust_policy",
                reason: "no policy grants this issuer and subject",
            });
        }

        Ok(FederatedGrant {
            issuer: claims.issuer.clone(),
            subject: claims.subject.clone(),
            allowances: winning
                .into_iter()
                .map(|policy| Allowance {
                    channels: policy.channels.clone(),
                    scopes: policy.scopes.clone(),
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    const US: &str = "weebo-notify";

    fn pattern(raw: &str) -> Pattern {
        Pattern::parse(raw).unwrap()
    }

    fn policy(subject: &str, channels: &[&str]) -> TrustPolicy {
        TrustPolicy {
            issuer: "https://forgejo.weebo.poc".to_owned(),
            subject: pattern(subject),
            channels: channels.iter().copied().map(pattern).collect(),
            scopes: vec![Scope::Publish],
            channel_scoped: false,
        }
    }

    fn claims(subject: &str) -> FederatedClaims {
        FederatedClaims {
            issuer: "https://forgejo.weebo.poc".to_owned(),
            subject: subject.to_owned(),
            audiences: vec![US.to_owned()],
            jti: Some("tok-1".to_owned()),
            expires_at: OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap(),
        }
    }

    // --- T11 · the anchored glob (design §3.4) ---------------------------

    #[test]
    fn critical_t11_a_wildcard_is_anchored_at_both_ends() {
        let subject = pattern("repo:weebo/*");

        assert!(subject.matches("repo:weebo/infra"));
        assert!(subject.matches("repo:weebo/infra:ref:refs/heads/main"));
        assert!(
            !subject.matches("repo:weebo-evil/x"),
            "a prefix comparison alone lets a lookalike org in — the whole point of T11"
        );
        assert!(!subject.matches("prefix-repo:weebo/infra"));
    }

    #[test]
    fn a_pattern_with_a_suffix_matches_both_ends_without_overlapping() {
        let subject = pattern("repo:weebo/*:ref:refs/heads/main");

        assert!(subject.matches("repo:weebo/infra:ref:refs/heads/main"));
        assert!(
            !subject.matches("repo:weebo/infra:ref:refs/heads/wip"),
            "the branch is part of the identity here"
        );
        assert!(
            !subject.matches("repo:weebo/:ref:refs/heads/mai"),
            "the two halves must not be allowed to overlap into a shorter string"
        );
    }

    #[test]
    fn a_pattern_that_would_trust_everything_is_refused_at_construction() {
        assert!(Pattern::parse("*").is_err(), "a bare `*` is not a policy");
        assert!(Pattern::parse("   ").is_err());
        assert!(
            Pattern::parse("repo:*/infra:*").is_err(),
            "two wildcards is a pattern nobody can predict; one is enough for §11"
        );
    }

    #[test]
    fn an_exact_pattern_matches_only_itself() {
        let subject = pattern("repo:weebo/infra:ref:refs/heads/main");

        assert!(subject.matches("repo:weebo/infra:ref:refs/heads/main"));
        assert!(!subject.matches("repo:weebo/infra:ref:refs/heads/main-2"));
    }

    // --- T11 · the audience, the jti, the policy -------------------------

    #[test]
    fn critical_t11_a_token_minted_for_another_service_is_refused() {
        let policies = TrustPolicies::new(vec![policy("repo:weebo/*", &["ci-*"])]);
        let foreign = FederatedClaims {
            audiences: vec!["some-other-service".to_owned()],
            ..claims("repo:weebo/infra")
        };

        assert!(
            matches!(
                policies.decide(&foreign, US).unwrap_err(),
                DomainError::Malformed {
                    field: "token.aud",
                    ..
                }
            ),
            "issuer and subject matching is not enough: the token has to be for us"
        );
    }

    #[test]
    fn critical_t11_a_token_that_cannot_be_remembered_is_refused() {
        let policies = TrustPolicies::new(vec![policy("repo:weebo/*", &["ci-*"])]);
        let anonymous = FederatedClaims {
            jti: None,
            ..claims("repo:weebo/infra")
        };

        assert!(
            matches!(
                policies.decide(&anonymous, US).unwrap_err(),
                DomainError::Malformed {
                    field: "token.jti",
                    ..
                }
            ),
            "without an id there is nothing to store, so replay protection would be \
             opt-out by omission"
        );
    }

    #[test]
    fn critical_t11_a_subject_no_policy_names_is_refused() {
        let policies = TrustPolicies::new(vec![policy("repo:weebo/*", &["ci-*"])]);

        assert!(matches!(
            policies
                .decide(&claims("repo:weebo-evil/x"), US)
                .unwrap_err(),
            DomainError::Malformed {
                field: "trust_policy",
                ..
            }
        ));
    }

    #[test]
    fn an_issuer_is_matched_exactly_and_never_as_a_pattern() {
        let policies = TrustPolicies::new(vec![policy("repo:weebo/*", &["ci-*"])]);
        let elsewhere = FederatedClaims {
            issuer: "https://forgejo.weebo.poc.evil".to_owned(),
            ..claims("repo:weebo/infra")
        };

        assert!(policies.decide(&elsewhere, US).is_err());
    }

    // --- T11 · channel policy beats global -------------------------------

    #[test]
    fn critical_t11_a_channel_scoped_policy_wins_over_the_global_one() {
        let mut scoped = policy("repo:weebo/*", &["ci-infra"]);
        scoped.channel_scoped = true;
        let policies = TrustPolicies::new(vec![policy("repo:weebo/*", &["ci-*"]), scoped]);

        let grant = policies.decide(&claims("repo:weebo/infra"), US).unwrap();

        assert!(grant.allows(&ChannelId::parse("ci-infra").unwrap(), Scope::Publish));
        assert!(
            !grant.allows(&ChannelId::parse("ci-web").unwrap(), Scope::Publish),
            "a channel that names its publishers has said who they are; a broad \
             global policy must not widen it"
        );
    }

    #[test]
    fn the_global_policy_applies_when_no_channel_names_this_subject() {
        let policies = TrustPolicies::new(vec![policy("repo:weebo/*", &["ci-*"])]);

        let grant = policies.decide(&claims("repo:weebo/infra"), US).unwrap();

        assert!(grant.allows(&ChannelId::parse("ci-web").unwrap(), Scope::Publish));
    }

    #[test]
    fn a_grant_carries_only_the_scopes_its_policy_gave() {
        let policies = TrustPolicies::new(vec![policy("repo:weebo/*", &["ci-*"])]);

        let grant = policies.decide(&claims("repo:weebo/infra"), US).unwrap();
        let channel = ChannelId::parse("ci-infra").unwrap();

        assert!(grant.allows(&channel, Scope::Publish));
        assert!(
            !grant.allows(&channel, Scope::Subscribe),
            "publishing is not reading, and a CI job asked for one of them"
        );
        assert!(!grant.allows(&channel, Scope::Manage));
    }

    #[test]
    fn a_channel_outside_every_pattern_is_not_granted() {
        let policies = TrustPolicies::new(vec![policy("repo:weebo/*", &["ci-*"])]);

        let grant = policies.decide(&claims("repo:weebo/infra"), US).unwrap();

        assert!(!grant.allows(&ChannelId::parse("cluster-ovh-1").unwrap(), Scope::Publish));
    }
}
