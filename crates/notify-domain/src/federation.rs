//! CI/CD federation (§5.3): deciding what a forge-vouched job may do. Signature
//! checks are the adapter's job. T11 hardening: the audience must name us, subject
//! globs are anchored, and a `jti` is required so a token is exchanged once.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::channel::Scope;
use crate::error::{DomainError, Result};
use crate::ids::ChannelId;

/// Claims verified against an issuer's JWKS; [`TrustPolicy`] decides what they allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederatedClaims {
    /// Who vouched for it, matched exactly against a policy's issuer.
    pub issuer: String,
    /// What the job says it is — `repo:weebo/infra:ref:refs/heads/main`.
    pub subject: String,
    /// Who the token was minted for. Must name this instance.
    pub audiences: Vec<String>,
    /// The token's own id, remembered until `accepted_until` so it cannot be replayed.
    pub jti: Option<String>,
    /// The last instant the verifier accepts the token: `exp` plus its leeway. Bounds
    /// how long the `jti` is remembered; the bare `exp` would reopen a replay window.
    pub accepted_until: OffsetDateTime,
}

/// One `[[trust_policy]]` or `[[channels.managed.publishers]]` block (§11); the
/// channel-scoped kind wins over global ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustPolicy {
    /// The issuer this policy trusts. Exact match.
    pub issuer: String,
    /// The subject pattern, matched **anchored** — see [`Pattern`].
    pub subject: Pattern,
    /// Which channels it grants, by pattern (`ci-*`).
    pub channels: Vec<Pattern>,
    /// What it grants there.
    pub scopes: Vec<Scope>,
    /// Whether this policy came from a channel's own `publishers` block (§11).
    pub channel_scoped: bool,
}

/// A glob with a single `*` wildcard, anchored at both ends.
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
    /// An empty pattern or a bare `*`, which would trust everything.
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

    /// Whether `candidate` matches as a whole (T11): `repo:weebo/*` never matches
    /// `repo:weebo-evil/x`, and prefix and suffix may not overlap.
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
    /// The issuer that vouched for the job, kept for the minted token and audit log.
    pub issuer: String,
    /// The subject, which becomes the `CiJob` principal (§5.1).
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

/// The set of policies this instance was configured with (§11).
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

    /// What was configured, read-only, for the platform console (§10.4bis).
    #[must_use]
    pub fn configured(&self) -> &[TrustPolicy] {
        &self.policies
    }

    /// Decide what an authenticated set of claims may do.
    ///
    /// # Errors
    ///
    /// In order: a token not minted for us (before policies, so it never reads as
    /// "no matching policy"), a token without `jti`, then no issuer+subject match.
    /// When any channel-scoped policy matches, global ones are dropped (§11).
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
            accepted_until: OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap(),
        }
    }

    // --- T11 · the anchored glob ------------------------------------------

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
