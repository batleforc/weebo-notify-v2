//! `POST /api/v1/auth/token/exchange` — a CI job trades its forge's ID token for one
//! of ours (§5.3): the policy match is resolved once and bounded to fifteen minutes.
//! Check order is the security: quota, signature, policy, then the replay claim.

use std::sync::Arc;

use notify_domain::{ChannelId, DomainError, FederatedGrant, Scope, TrustPolicies};
use notify_ports::{
    Allowance, ChannelRepository, Clock, Failure, FederationVerifier, MintedToken, Quota,
    RateLimiter, ReplayGuard,
};

/// The trust policies in force, swappable on `SIGHUP` (§5.3, §11). A `RwLock` is
/// enough: it is read once per token exchange.
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

    /// Replace the whole set in force (§11); never merge, so a withdrawn policy stops granting.
    pub fn adopt(&self, policies: TrustPolicies) {
        // Write through a poisoned lock: refusing would keep a withdrawn policy in force.
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

    pub async fn execute(&self, presented: &str) -> Result<MintedToken, Failure> {
        // The endpoint is unauthenticated, so the global limit in front of it guards the
        // signature check; the per-(issuer, subject) quota (§8.2) follows once claims are known.
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
            // No channel override: no channel is known yet (§5.3).
        } = self.limiter.check(&quota, None, self.clock.now()).await?
        {
            return Err(Failure::RateLimited {
                retry_after_seconds,
            });
        }

        // Audience, subject glob, channel-scoped precedence, in one pure call (§5.3, T11).
        // Read per exchange so a `SIGHUP` takes effect.
        let grant = self.policies.current().decide(&claims, &self.audience)?;

        // Resolve the patterns now, against the channels that exist now (§11).
        let grants = self.resolve(&grant).await?;
        if grants.is_empty() {
            // A policy granting nothing reachable is a configuration mistake: say so.
            return Err(DomainError::Malformed {
                field: "trust_policy.channels",
                reason: "no channel on this instance matches what the policy grants",
            }
            .into());
        }

        // Only after the policy matched, so a non-matching token cannot burn a real job's id.
        let jti = claims.jti.as_deref().unwrap_or_default();
        if !self.replay.remember(jti, claims.expires_at).await? {
            tracing::info!(subject = %claims.subject, "ci token replayed");
            return Err(DomainError::InvalidCredential.into());
        }

        Ok(self.federation.mint(&grant, &grants).await?)
    }

    /// Every existing channel the grant's patterns cover, each with only the scopes the
    /// allowances covering it grant.
    async fn resolve(
        &self,
        grant: &FederatedGrant,
    ) -> Result<Vec<(ChannelId, Vec<Scope>)>, Failure> {
        let mut resolved = Vec::new();
        for channel in self.channels.all_ids().await? {
            let mut scopes: Vec<Scope> = [Scope::Publish, Scope::Subscribe, Scope::Manage]
                .into_iter()
                .filter(|scope| grant.allows(&channel, *scope))
                .collect();
            scopes.sort_unstable();
            if !scopes.is_empty() {
                resolved.push((channel, scopes));
            }
        }
        Ok(resolved)
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
        /// The same handle the exchange reads, so a test can swap under it.
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

    // --- Trust policies swapped while the process runs (§11) ----------------

    #[tokio::test]
    async fn a_withdrawn_policy_stops_granting_without_a_restart() {
        let fixture = fixture(vec![policy("repo:weebo/*", &["ci-*"])], &["ci-infra"]);
        fixture.federation.register("first", claims("tok-1"));
        fixture.federation.register("second", claims("tok-2"));

        assert!(fixture.exchange.execute("first").await.is_ok());

        // Withdrawing a policy must take effect without a restart.
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
        // The policy refuses this subject; the `jti` must stay available.
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

    #[tokio::test]
    async fn two_policies_do_not_add_up_to_both_scopes_everywhere() {
        let publish = policy("repo:weebo/*", &["ci-*"]);
        let manage = TrustPolicy {
            scopes: vec![Scope::Manage],
            ..policy("repo:weebo/infra:*", &["infra-admin"])
        };
        let fixture = fixture(vec![publish, manage], &["ci-infra", "infra-admin"]);
        fixture.federation.register("job", claims("tok-pair"));

        let minted = fixture.exchange.execute("job").await.unwrap();
        let claims = fixture
            .federation
            .verify_minted(&minted.token)
            .await
            .unwrap()
            .unwrap();

        assert!(
            claims
                .grants
                .contains(&(ChannelId::parse("ci-infra").unwrap(), vec![Scope::Publish])),
            "ci-* only ever got publish"
        );
        assert!(claims.grants.contains(&(
            ChannelId::parse("infra-admin").unwrap(),
            vec![Scope::Manage]
        )));
    }
}
