//! `POST /api/v1/auth/token/exchange` — a CI job trades its forge's ID token for
//! one of ours (design §5.3).
//!
//! # Why an exchange rather than accepting the forge's token directly
//!
//! Accepting it directly would work, and would be worse in three ways that only
//! show up later. The forge's token is minted for whatever audience the forge
//! chose, lives as long as the forge decided, and says nothing about *this*
//! instance's channels — so every request would re-run the policy match against
//! configuration that may have been reloaded since (§11, `reload`), and two
//! requests of one pipeline could legitimately disagree. Minting resolves the
//! question once, writes the answer down, and bounds it to fifteen minutes.
//!
//! # The order of the checks is the security
//!
//! Quota, then signature, then policy, then replay — each one placed where it is
//! for a reason stated at the call site. The one worth naming here: the replay
//! guard is claimed **after** the policy matches and **before** anything is minted.
//! Claiming earlier would let anyone with a valid-looking token burn other jobs'
//! ids; claiming later would leave a window where two concurrent requests both
//! mint.

use std::sync::Arc;

use notify_domain::{ChannelId, DomainError, FederatedGrant, Scope, TrustPolicies};
use notify_ports::{
    Allowance, ChannelRepository, Clock, Failure, FederationVerifier, MintedToken, Quota,
    RateLimiter, ReplayGuard,
};

/// The trust policies in force, swappable while the process runs (§5.3, §11, D31).
///
/// # Why this is not just a `TrustPolicies`
///
/// `[[trust_policy]]` decides which forge may publish here, and §11 says the
/// configuration is re-read on `SIGHUP`. Held by value, it could not be: the set
/// was cloned into this use case at boot, so a reload changed a file and nothing
/// else. M4duodetricies made that safe by *not* pretending — a reload kept the
/// running set and warned — and left the real fix here (D31).
///
/// # Why a lock and not an `ArcSwap`
///
/// Reads happen once per token exchange, which is a CI job logging in, not a hot
/// path: a `RwLock` costs nothing measurable here and costs no dependency either.
/// The same reasoning, and the same poisoning policy, as `ReconciliationLog`.
#[derive(Debug, Clone, Default)]
pub struct LivePolicies {
    current: Arc<std::sync::RwLock<TrustPolicies>>,
}

impl LivePolicies {
    /// Start from what the file said at boot.
    #[must_use]
    pub fn new(policies: TrustPolicies) -> Self {
        Self {
            current: Arc::new(std::sync::RwLock::new(policies)),
        }
    }

    /// Replace the set in force — what a `SIGHUP` does (§11).
    ///
    /// Whole-set replacement, never a merge: a policy withdrawn from the file has to
    /// stop granting anything, and that is the half a merge would lose. It is also
    /// the half that matters, since it is the one that *removes* a right.
    pub fn adopt(&self, policies: TrustPolicies) {
        // A poisoned lock means a previous writer panicked mid-write. Writing through
        // it is the safe choice here and it is worth saying why: the alternative is
        // refusing the swap, which would leave a **withdrawn** policy in force for
        // the life of the process.
        match self.current.write() {
            Ok(mut slot) => *slot = policies,
            Err(poisoned) => *poisoned.into_inner() = policies,
        }
    }

    /// The set in force right now.
    #[must_use]
    pub fn current(&self) -> TrustPolicies {
        match self.current.read() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// Exchange a forge ID token for a short-lived weebo-notify token.
#[derive(Clone)]
pub struct ExchangeToken {
    federation: Arc<dyn FederationVerifier>,
    replay: Arc<dyn ReplayGuard>,
    channels: Arc<dyn ChannelRepository>,
    limiter: Arc<dyn RateLimiter>,
    clock: Arc<dyn Clock>,
    policies: LivePolicies,
    audience: String,
}

impl core::fmt::Debug for ExchangeToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ExchangeToken").finish_non_exhaustive()
    }
}

impl ExchangeToken {
    /// Wire it.
    #[must_use]
    pub fn new(
        federation: Arc<dyn FederationVerifier>,
        replay: Arc<dyn ReplayGuard>,
        channels: Arc<dyn ChannelRepository>,
        limiter: Arc<dyn RateLimiter>,
        clock: Arc<dyn Clock>,
        policies: LivePolicies,
        audience: String,
    ) -> Self {
        Self {
            federation,
            replay,
            channels,
            limiter,
            clock,
            policies,
            audience,
        }
    }

    /// Exchange.
    pub async fn execute(&self, presented: &str) -> Result<MintedToken, Failure> {
        // The signature comes first among the *checks*, but the quota comes first
        // among everything: verifying a JWT is cheap and looking a JWKS up is not,
        // and this endpoint is unauthenticated by construction — anything reachable
        // without a credential is reachable by anyone.
        //
        // It is keyed on the unverified claims below rather than here, though,
        // because "(issuer, subject)" is what §8.2 says to limit and neither is
        // known yet. What guards this line is the global limit in front of it.
        let claims = match self.federation.verify_ci_token(presented).await? {
            Ok(claims) => claims,
            Err(rejection) => {
                tracing::info!(reason = rejection.as_str(), "ci token refused");
                return Err(DomainError::InvalidCredential.into());
            }
        };

        let quota = Quota::TokenExchange {
            subject: format!("{}|{}", claims.issuer, claims.subject),
        };
        if let Allowance::Limited {
            retry_after_seconds,
            // No override: the exchange happens before any channel is known — that is
            // the point of checking it first (§5.3).
        } = self.limiter.check(&quota, None, self.clock.now()).await?
        {
            return Err(Failure::RateLimited {
                retry_after_seconds,
            });
        }

        // Audience, anchored subject glob, channel-scoped precedence — all of it,
        // in one pure call with its own tests (design §5.3, scenario T11).
        // Read once per exchange rather than held: this is what makes a `SIGHUP`
        // reach the path that decides who may publish (D31).
        let grant = self.policies.current().decide(&claims, &self.audience)?;

        // Resolve the patterns *now*, against the channels that exist now. A token
        // carrying `ci-*` would push this work onto every later request, against a
        // configuration that may have been reloaded in between (§11).
        let channels = self.resolve(&grant).await?;
        if channels.is_empty() {
            // A policy that matches but grants nothing reachable is a configuration
            // mistake, and saying so beats minting a token that can do nothing and
            // letting the job discover it one 403 at a time.
            return Err(DomainError::Malformed {
                field: "trust_policy.channels",
                reason: "no channel on this instance matches what the policy grants",
            }
            .into());
        }
        let scopes = Self::scopes_of(&grant);

        // Now, and not before: an id burned by a token that turned out not to match
        // any policy would be a way to lock a real job out of its own exchange.
        let jti = claims.jti.as_deref().unwrap_or_default();
        if !self.replay.remember(jti, claims.expires_at).await? {
            tracing::info!(subject = %claims.subject, "ci token replayed");
            return Err(DomainError::InvalidCredential.into());
        }

        Ok(self.federation.mint(&grant, &channels, &scopes).await?)
    }

    /// Every existing channel the grant's patterns cover.
    ///
    /// Reads the channel list rather than trusting the patterns: a policy naming
    /// `ci-*` on an instance with no such channel must produce a token that says so,
    /// not one that looks broad and does nothing.
    async fn resolve(&self, grant: &FederatedGrant) -> Result<Vec<ChannelId>, Failure> {
        let mut resolved: Vec<ChannelId> = Vec::new();
        for channel in self.channels.all_ids().await? {
            let covered = grant.allowances.iter().any(|allowance| {
                allowance
                    .channels
                    .iter()
                    .any(|pattern| pattern.matches(channel.as_str()))
            });
            if covered && !resolved.contains(&channel) {
                resolved.push(channel);
            }
        }
        Ok(resolved)
    }

    /// The union of what the winning policies grant.
    fn scopes_of(grant: &FederatedGrant) -> Vec<Scope> {
        let mut scopes: Vec<Scope> = Vec::new();
        for allowance in &grant.allowances {
            for scope in &allowance.scopes {
                if !scopes.contains(scope) {
                    scopes.push(*scope);
                }
            }
        }
        scopes.sort_unstable();
        scopes
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{Channel, FederatedClaims, Pattern, TrustPolicy};
    use notify_ports::test_utils::{
        InMemory, InMemoryReplayGuard, StaticFederationVerifier, TestClock, TestRateLimiter,
    };
    use time::OffsetDateTime;

    use super::*;

    const US: &str = "weebo-notify";
    const FORGE: &str = "https://forgejo.weebo.poc";

    struct Fixture {
        exchange: ExchangeToken,
        federation: StaticFederationVerifier,
        limiter: TestRateLimiter,
        /// The same handle the exchange reads, so a test can swap under it (D31).
        policies: LivePolicies,
    }

    fn claims(jti: &str) -> FederatedClaims {
        FederatedClaims {
            issuer: FORGE.to_owned(),
            subject: "repo:weebo/infra:ref:refs/heads/main".to_owned(),
            audiences: vec![US.to_owned()],
            jti: Some(jti.to_owned()),
            expires_at: OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap(),
        }
    }

    fn fixture(policies: Vec<TrustPolicy>, channels: &[&str]) -> Fixture {
        let store = InMemory::new();
        for id in channels {
            store.seed_channel(Channel::dynamic(ChannelId::parse(id).unwrap(), None).unwrap());
        }
        let federation = StaticFederationVerifier::new();
        let limiter = TestRateLimiter::default();
        let live = LivePolicies::new(TrustPolicies::new(policies));

        Fixture {
            exchange: ExchangeToken::new(
                Arc::new(federation.clone()),
                Arc::new(InMemoryReplayGuard::new()),
                Arc::new(store),
                Arc::new(limiter.clone()),
                Arc::new(TestClock::new()),
                live.clone(),
                US.to_owned(),
            ),
            federation,
            limiter,
            policies: live,
        }
    }

    fn policy(subject: &str, channels: &[&str]) -> TrustPolicy {
        TrustPolicy {
            issuer: FORGE.to_owned(),
            subject: Pattern::parse(subject).unwrap(),
            channels: channels
                .iter()
                .map(|one| Pattern::parse(one).unwrap())
                .collect(),
            scopes: vec![Scope::Publish],
            channel_scoped: false,
        }
    }

    // --- Trust policies swapped while the process runs (§11, D31) -----------

    #[tokio::test]
    async fn a_withdrawn_policy_stops_granting_without_a_restart() {
        let fixture = fixture(vec![policy("repo:weebo/*", &["ci-*"])], &["ci-infra"]);
        fixture.federation.register("first", claims("tok-1"));
        fixture.federation.register("second", claims("tok-2"));

        assert!(fixture.exchange.execute("first").await.is_ok());

        // What D31 was: `[[trust_policy]]` was cloned into this use case at boot, so
        // withdrawing it from the file changed a file and nothing else — the forge
        // went on publishing until somebody restarted the process.
        fixture.policies.adopt(TrustPolicies::new(Vec::new()));

        assert!(
            fixture.exchange.execute("second").await.is_err(),
            "a policy withdrawn from the configuration must stop granting at once"
        );
    }

    #[tokio::test]
    async fn a_policy_added_at_runtime_grants_without_a_restart() {
        let fixture = fixture(Vec::new(), &["ci-infra"]);
        fixture.federation.register("forge-token", claims("tok-1"));

        assert!(fixture.exchange.execute("forge-token").await.is_err());

        fixture
            .policies
            .adopt(TrustPolicies::new(vec![policy("repo:weebo/*", &["ci-*"])]));

        let minted = fixture.exchange.execute("forge-token").await.unwrap();
        assert_eq!(minted.channels, vec![ChannelId::parse("ci-infra").unwrap()]);
    }

    #[tokio::test]
    async fn an_exchange_resolves_the_patterns_to_the_channels_that_exist() {
        let fixture = fixture(
            vec![policy("repo:weebo/*", &["ci-*"])],
            &["ci-infra", "ci-web", "cluster-ovh-1"],
        );
        fixture.federation.register("forge-token", claims("tok-1"));

        let minted = fixture.exchange.execute("forge-token").await.unwrap();

        assert_eq!(
            minted.channels,
            vec![
                ChannelId::parse("ci-infra").unwrap(),
                ChannelId::parse("ci-web").unwrap()
            ],
            "the token says which channels, so no later request re-runs the match"
        );
        assert_eq!(minted.scopes, vec![Scope::Publish]);
    }

    #[tokio::test]
    async fn critical_t11_the_same_token_exchanges_exactly_once() {
        let fixture = fixture(vec![policy("repo:weebo/*", &["ci-*"])], &["ci-infra"]);
        fixture.federation.register("forge-token", claims("tok-1"));

        assert!(fixture.exchange.execute("forge-token").await.is_ok());
        assert!(
            fixture.exchange.execute("forge-token").await.is_err(),
            "a leaked ID token is worth one exchange, not fifteen minutes of them"
        );
        assert_eq!(
            fixture.federation.minted(),
            1,
            "and the second attempt must not have minted anything"
        );
    }

    #[tokio::test]
    async fn critical_t11_a_token_for_another_audience_is_refused() {
        let fixture = fixture(vec![policy("repo:weebo/*", &["ci-*"])], &["ci-infra"]);
        fixture.federation.register(
            "forge-token",
            FederatedClaims {
                audiences: vec!["some-other-service".to_owned()],
                ..claims("tok-1")
            },
        );

        assert!(fixture.exchange.execute("forge-token").await.is_err());
        assert_eq!(fixture.federation.minted(), 0);
    }

    #[tokio::test]
    async fn critical_t11_a_subject_outside_the_anchored_glob_is_refused() {
        let fixture = fixture(vec![policy("repo:weebo/*", &["ci-*"])], &["ci-infra"]);
        fixture.federation.register(
            "forge-token",
            FederatedClaims {
                subject: "repo:weebo-evil/x".to_owned(),
                ..claims("tok-1")
            },
        );

        assert!(fixture.exchange.execute("forge-token").await.is_err());
        assert_eq!(fixture.federation.minted(), 0);
    }

    #[tokio::test]
    async fn a_refused_token_does_not_burn_the_id_of_a_real_one() {
        // The policy refuses this subject; the `jti` must stay available, or a
        // stranger could lock a real job out of its own exchange by presenting a
        // token carrying its id.
        let fixture = fixture(vec![policy("repo:weebo/infra*", &["ci-*"])], &["ci-infra"]);
        fixture.federation.register(
            "stranger",
            FederatedClaims {
                subject: "repo:weebo/other".to_owned(),
                ..claims("shared-id")
            },
        );
        fixture.federation.register("real", claims("shared-id"));

        assert!(fixture.exchange.execute("stranger").await.is_err());
        assert!(
            fixture.exchange.execute("real").await.is_ok(),
            "a refusal must not consume the id it was refused on"
        );
    }

    #[tokio::test]
    async fn an_unknown_token_is_an_invalid_credential_and_not_a_policy_error() {
        let fixture = fixture(vec![policy("repo:weebo/*", &["ci-*"])], &["ci-infra"]);

        assert_eq!(
            fixture
                .exchange
                .execute("never-issued")
                .await
                .unwrap_err()
                .code(),
            Some("auth.invalid_credential")
        );
    }

    #[tokio::test]
    async fn a_policy_matching_no_existing_channel_says_so() {
        let fixture = fixture(vec![policy("repo:weebo/*", &["ci-*"])], &["cluster-ovh-1"]);
        fixture.federation.register("forge-token", claims("tok-1"));

        assert!(
            fixture.exchange.execute("forge-token").await.is_err(),
            "minting a token that can do nothing postpones the mistake to the job"
        );
        assert_eq!(fixture.federation.minted(), 0);
    }

    #[tokio::test]
    async fn the_exchange_is_rate_limited_per_issuer_and_subject() {
        let fixture = fixture(vec![policy("repo:weebo/*", &["ci-*"])], &["ci-infra"]);
        fixture.federation.register("forge-token", claims("tok-1"));
        fixture.limiter.limit(Quota::TokenExchange {
            subject: format!("{FORGE}|repo:weebo/infra:ref:refs/heads/main"),
        });

        assert!(matches!(
            fixture.exchange.execute("forge-token").await.unwrap_err(),
            Failure::RateLimited { .. }
        ));
        assert_eq!(fixture.federation.minted(), 0);
    }
}
