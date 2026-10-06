//! Doubles for the stateless infrastructure ports.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use notify_domain::{
    ApiKeyPrefix, AttachmentId, ChannelId, FederatedClaims, FederatedGrant, Scope,
};
use time::{Duration, OffsetDateTime};
use url::Url;

use crate::attachments::{
    AttachmentStore, PresignedDownload, PresignedUpload, StoredObject, UploadRequest,
};
use crate::clock::Clock;
use crate::error::{PortError, PortResult};
use crate::identities::{KeyGenerator, SecretHasher};
use crate::leader::{LeaderElection, LeaderGuard};
use crate::quota::{Allowance, Quota, RateLimiter, StreamLeases};
use crate::sinks::{SinkDelivery, SinkDispatcher, SinkOutcome};
use crate::tokens::{
    FederationVerifier, MintedClaims, MintedToken, ReplayGuard, TokenRejection, TokenVerifier,
    VerifiedClaims,
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock means an earlier assertion failed; read through so that one is reported.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- Secret hashing --------------------------------------------------------

/// A hasher that is not one: keeps Argon2id cost out of test suites.
#[derive(Debug, Clone, Copy, Default)]
pub struct StubSecretHasher;

#[async_trait::async_trait]
impl SecretHasher for StubSecretHasher {
    async fn hash(&self, secret: &str) -> PortResult<String> {
        Ok(format!("stub${secret}"))
    }

    async fn verify(&self, secret: &str, hash: &str) -> PortResult<bool> {
        Ok(hash == format!("stub${secret}"))
    }
}

// --- Token verification ----------------------------------------------------

/// A verifier with a fixed table of tokens; unknown tokens are
/// [`TokenRejection::BadSignature`].
#[derive(Debug, Clone)]
pub struct StaticTokenVerifier {
    tokens: Arc<Mutex<Vec<(String, VerifiedClaims)>>>,
    clock: Arc<dyn Clock>,
}

impl StaticTokenVerifier {
    /// An empty verifier reading the real clock: every token is refused.
    pub fn new() -> Self {
        Self::with_clock(Arc::new(crate::clock::SystemClock))
    }

    /// An empty verifier reading the clock a test controls, for expiry checks.
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            tokens: Arc::new(Mutex::new(Vec::new())),
            clock,
        }
    }

    /// Register a token and the claims it stands for.
    pub fn register(&self, token: &str, claims: VerifiedClaims) -> &Self {
        lock(&self.tokens).push((token.to_owned(), claims));
        self
    }
}

impl Default for StaticTokenVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl TokenVerifier for StaticTokenVerifier {
    async fn verify(&self, token: &str) -> PortResult<Result<VerifiedClaims, TokenRejection>> {
        let found = lock(&self.tokens)
            .iter()
            .find(|(candidate, _)| candidate == token)
            .map(|(_, claims)| claims.clone());

        Ok(match found {
            Some(claims) if claims.expires_at <= self.clock.now() => Err(TokenRejection::Expired),
            Some(claims) => Ok(claims),
            None => Err(TokenRejection::BadSignature),
        })
    }

    async fn health(&self) -> PortResult<()> {
        Ok(())
    }
}

// --- Federation ------------------------------------------------------------

/// A federation verifier a test hands tokens to. Like the real one it only reports
/// claims (§5.3); register claims with the wrong audience to test refusal.
#[derive(Clone)]
pub struct StaticFederationVerifier {
    tokens: Arc<Mutex<Vec<(String, FederatedClaims)>>>,
    issued: Arc<Mutex<Vec<(String, MintedClaims)>>>,
    minted: Arc<Mutex<u32>>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for StaticFederationVerifier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StaticFederationVerifier")
            .finish_non_exhaustive()
    }
}

impl StaticFederationVerifier {
    /// An empty one reading the real clock: every token is unknown.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Arc::new(crate::clock::SystemClock))
    }

    /// An empty one reading the clock a test controls; minting stamps `expires_at` against it.
    #[must_use]
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            tokens: Arc::new(Mutex::new(Vec::new())),
            issued: Arc::new(Mutex::new(Vec::new())),
            minted: Arc::new(Mutex::new(0)),
            clock,
        }
    }

    /// Teach it one token.
    pub fn register(&self, token: &str, claims: FederatedClaims) -> &Self {
        lock(&self.tokens).push((token.to_owned(), claims));
        self
    }

    /// How many tokens it has minted.
    #[must_use]
    pub fn minted(&self) -> u32 {
        *lock(&self.minted)
    }
}

impl Default for StaticFederationVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl FederationVerifier for StaticFederationVerifier {
    async fn verify_ci_token(
        &self,
        token: &str,
    ) -> PortResult<Result<FederatedClaims, TokenRejection>> {
        Ok(lock(&self.tokens)
            .iter()
            .find(|(known, _)| known == token)
            .map(|(_, claims)| claims.clone())
            .ok_or(TokenRejection::BadSignature))
    }

    async fn mint(
        &self,
        grant: &FederatedGrant,
        grants: &[(ChannelId, Vec<Scope>)],
    ) -> PortResult<MintedToken> {
        let mut minted = lock(&self.minted);
        *minted += 1;
        let token = format!("test-ci-token-{minted}");
        let expires_at = self.clock.now() + Duration::minutes(15);

        // Remembered so `verify_minted` can answer.
        lock(&self.issued).push((
            token.clone(),
            MintedClaims {
                issuer: Url::parse(&grant.issuer)
                    .unwrap_or_else(|_| Url::parse("https://forge.invalid").expect("a valid url")),
                subject: grant.subject.clone(),
                grants: grants.to_vec(),
                expires_at,
            },
        ));

        Ok(MintedToken::new(token, expires_at, grants))
    }

    async fn verify_minted(&self, token: &str) -> PortResult<Result<MintedClaims, TokenRejection>> {
        Ok(lock(&self.issued)
            .iter()
            .find(|(known, _)| known == token)
            .map(|(_, claims)| claims.clone())
            .ok_or(TokenRejection::BadSignature))
    }
}

/// Replay memory in a `Vec`; keeps expired entries until `prune`, like the real store.
#[derive(Debug, Clone, Default)]
pub struct InMemoryReplayGuard {
    seen: Arc<Mutex<Vec<(String, OffsetDateTime)>>>,
}

impl InMemoryReplayGuard {
    /// An empty one.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ReplayGuard for InMemoryReplayGuard {
    async fn remember(&self, jti: &str, until: OffsetDateTime) -> PortResult<bool> {
        let mut seen = lock(&self.seen);
        if seen.iter().any(|(known, _)| known == jti) {
            return Ok(false);
        }
        seen.push((jti.to_owned(), until));
        Ok(true)
    }

    async fn prune(&self, now: OffsetDateTime) -> PortResult<u64> {
        let mut seen = lock(&self.seen);
        let before = seen.len();
        seen.retain(|(_, until)| *until > now);
        Ok((before - seen.len()) as u64)
    }
}

// --- Leader election -------------------------------------------------------

/// A leader election a test decides the outcome of.
#[derive(Debug, Clone)]
pub struct TestLeaderElection {
    leader: Arc<Mutex<bool>>,
}

impl TestLeaderElection {
    /// This replica wins every election.
    pub fn leader() -> Self {
        Self {
            leader: Arc::new(Mutex::new(true)),
        }
    }

    /// This replica wins none.
    pub fn follower() -> Self {
        Self {
            leader: Arc::new(Mutex::new(false)),
        }
    }

    /// Take leadership away mid-run (a partition); holders must notice via
    /// [`LeaderGuard::is_held`] (T22).
    pub fn depose(&self) {
        *lock(&self.leader) = false;
    }
}

#[derive(Debug)]
struct TestGuard {
    leader: Arc<Mutex<bool>>,
}

#[async_trait::async_trait]
impl LeaderGuard for TestGuard {
    async fn is_held(&self) -> bool {
        *lock(&self.leader)
    }

    async fn release(&self) {
        // Releasing the lock is not losing the election: the real one re-takes it next
        // tick. Losing leadership is `depose`.
    }
}

#[async_trait::async_trait]
impl LeaderElection for TestLeaderElection {
    async fn try_acquire(&self, _job: &'static str) -> PortResult<Option<Box<dyn LeaderGuard>>> {
        if *lock(&self.leader) {
            Ok(Some(Box::new(TestGuard {
                leader: Arc::clone(&self.leader),
            })))
        } else {
            Ok(None)
        }
    }
}

// --- Attachments -----------------------------------------------------------

/// An object store that signs URLs nobody can upload to, and remembers what it was asked for.
#[derive(Debug, Clone, Default)]
pub struct InMemoryAttachments {
    uploaded: Arc<Mutex<Vec<AttachmentId>>>,
    /// Bytes the server wrote itself — the avatar path (design §2.4).
    objects: Arc<Mutex<std::collections::HashMap<AttachmentId, StoredObject>>>,
    /// Thumbnails the server derived itself, kept apart from `objects`.
    thumbnails: Arc<Mutex<std::collections::HashMap<AttachmentId, StoredObject>>>,
}

impl InMemoryAttachments {
    /// An empty bucket.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pretend an upload completed, so [`AttachmentStore::exists`] says yes.
    pub fn mark_uploaded(&self, id: AttachmentId) {
        lock(&self.uploaded).push(id);
    }

    /// Everything marked uploaded.
    pub fn uploaded(&self) -> Vec<AttachmentId> {
        lock(&self.uploaded).clone()
    }
}

#[async_trait::async_trait]
impl AttachmentStore for InMemoryAttachments {
    async fn presign_upload(&self, request: &UploadRequest) -> PortResult<PresignedUpload> {
        let id = AttachmentId::generate();
        let url = Url::parse(&format!("https://bucket.test/{id}"))
            .map_err(|error| PortError::corrupt("attachment.url", error))?;

        Ok(PresignedUpload {
            id,
            url,
            headers: [
                ("content-type".to_owned(), request.content_type.clone()),
                (
                    "content-length".to_owned(),
                    request.content_length.to_string(),
                ),
            ]
            .into(),
            expires_at: OffsetDateTime::now_utc() + Duration::minutes(15),
        })
    }

    async fn presign_download(&self, id: AttachmentId) -> PortResult<PresignedDownload> {
        Ok(PresignedDownload {
            url: Url::parse(&format!("https://bucket.test/{id}"))
                .map_err(|error| PortError::corrupt("attachment.url", error))?,
            expires_at: OffsetDateTime::now_utc() + Duration::minutes(15),
        })
    }

    async fn exists(&self, id: AttachmentId) -> PortResult<bool> {
        Ok(lock(&self.uploaded).contains(&id) || lock(&self.objects).contains_key(&id))
    }

    async fn delete(&self, id: AttachmentId) -> PortResult<()> {
        lock(&self.uploaded).retain(|candidate| *candidate != id);
        lock(&self.objects).remove(&id);
        lock(&self.thumbnails).remove(&id);
        Ok(())
    }

    async fn put(&self, id: AttachmentId, content_type: &str, bytes: Vec<u8>) -> PortResult<()> {
        lock(&self.objects).insert(
            id,
            StoredObject {
                content_type: content_type.to_owned(),
                bytes,
            },
        );
        Ok(())
    }

    async fn get(&self, id: AttachmentId) -> PortResult<Option<StoredObject>> {
        Ok(lock(&self.objects).get(&id).cloned())
    }

    async fn put_thumbnail(
        &self,
        id: AttachmentId,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> PortResult<()> {
        lock(&self.thumbnails).insert(
            id,
            StoredObject {
                content_type: content_type.to_owned(),
                bytes,
            },
        );
        Ok(())
    }

    async fn get_thumbnail(&self, id: AttachmentId) -> PortResult<Option<StoredObject>> {
        Ok(lock(&self.thumbnails).get(&id).cloned())
    }

    async fn health(&self) -> PortResult<()> {
        Ok(())
    }
}

// --- Sinks -----------------------------------------------------------------

/// A sink dispatcher that records what it was handed and answers as the test says.
#[derive(Debug, Clone, Default)]
pub struct RecordingSinkDispatcher {
    delivered: Arc<Mutex<Vec<SinkDelivery>>>,
    /// The targets it was handed, to assert secrets come from the registry at send time.
    targets: Arc<Mutex<Vec<crate::sinks::SinkTarget>>>,
    outcome: Arc<Mutex<Option<SinkOutcome>>>,
}

impl RecordingSinkDispatcher {
    /// A dispatcher that accepts everything.
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every subsequent delivery come back with this outcome (T17).
    pub fn always(&self, outcome: SinkOutcome) -> &Self {
        *lock(&self.outcome) = Some(outcome);
        self
    }

    /// Everything it was asked to deliver, in order.
    pub fn delivered(&self) -> Vec<SinkDelivery> {
        lock(&self.delivered).clone()
    }

    /// The targets those deliveries were resolved to, in the same order.
    pub fn targets(&self) -> Vec<crate::sinks::SinkTarget> {
        lock(&self.targets).clone()
    }
}

#[async_trait::async_trait]
impl SinkDispatcher for RecordingSinkDispatcher {
    fn handles(&self) -> &'static [&'static str] {
        &["webhook", "email"]
    }

    async fn deliver(
        &self,
        delivery: &SinkDelivery,
        target: &crate::sinks::SinkTarget,
    ) -> PortResult<SinkOutcome> {
        lock(&self.targets).push(target.clone());
        lock(&self.delivered).push(delivery.clone());
        Ok(lock(&self.outcome)
            .clone()
            .unwrap_or(SinkOutcome::Delivered))
    }
}

/// A hand-written routing table standing in for `[[channels.managed.sinks]]` (§11).
#[derive(Debug, Clone, Default)]
pub struct StaticSinkRegistry {
    routes: Arc<Mutex<Vec<(notify_domain::ChannelId, crate::sinks::SinkTarget)>>>,
}

impl StaticSinkRegistry {
    /// One with nothing routed anywhere.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Route a channel to a target. Returns `self` so a fixture reads as a list.
    #[must_use]
    pub fn route(
        self,
        channel: notify_domain::ChannelId,
        target: crate::sinks::SinkTarget,
    ) -> Self {
        lock(&self.routes).push((channel, target));
        self
    }

    /// Forget a target, as if removed from config while a delivery was queued.
    pub fn withdraw(&self, sink: &notify_domain::SinkId) {
        lock(&self.routes).retain(|(_, target)| target.id().as_ref() != Ok(sink));
    }
}

impl crate::sinks::SinkRegistry for StaticSinkRegistry {
    fn sinks_for(&self, channel: &notify_domain::ChannelId) -> Vec<notify_domain::SinkId> {
        lock(&self.routes)
            .iter()
            .filter(|(id, _)| id == channel)
            .filter_map(|(_, target)| target.id().ok())
            .collect()
    }

    fn target(
        &self,
        channel: &notify_domain::ChannelId,
        sink: &notify_domain::SinkId,
    ) -> Option<crate::sinks::SinkTarget> {
        lock(&self.routes)
            .iter()
            .find(|(id, target)| id == channel && target.id().as_ref() == Ok(sink))
            .map(|(_, target)| target.clone())
    }

    fn is_empty(&self) -> bool {
        lock(&self.routes).is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    fn claims(expires_at: OffsetDateTime) -> VerifiedClaims {
        VerifiedClaims {
            issuer: Url::parse("https://auth.weebo.poc").unwrap(),
            subject: "ak-user-42".to_owned(),
            email: None,
            display_name: None,
            groups: vec!["dev-team".to_owned()],
            expires_at,
        }
    }

    #[tokio::test]
    async fn the_stub_hasher_verifies_only_the_secret_it_hashed() {
        let hasher = StubSecretHasher;
        let hash = hasher.hash("s3cr3t").await.unwrap();

        assert!(hasher.verify("s3cr3t", &hash).await.unwrap());
        assert!(!hasher.verify("wrong", &hash).await.unwrap());
    }

    #[tokio::test]
    async fn an_unregistered_token_is_refused_rather_than_waved_through() {
        let verifier = StaticTokenVerifier::new();
        assert_eq!(
            verifier.verify("whatever").await.unwrap(),
            Err(TokenRejection::BadSignature)
        );
    }

    #[tokio::test]
    async fn a_registered_but_expired_token_is_still_refused() {
        let verifier = StaticTokenVerifier::new();
        verifier.register(
            "stale",
            claims(OffsetDateTime::now_utc() - Duration::hours(1)),
        );
        verifier.register(
            "fresh",
            claims(OffsetDateTime::now_utc() + Duration::hours(1)),
        );

        assert_eq!(
            verifier.verify("stale").await.unwrap(),
            Err(TokenRejection::Expired)
        );
        assert!(verifier.verify("fresh").await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn a_follower_never_acquires_and_a_deposed_leader_notices() {
        let follower = TestLeaderElection::follower();
        assert!(follower.try_acquire("purge").await.unwrap().is_none());

        let leader = TestLeaderElection::leader();
        let guard = leader.try_acquire("purge").await.unwrap().unwrap();
        assert!(guard.is_held().await);

        // Releasing at the end of a pass leaves the election won.
        guard.release().await;
        assert!(leader.try_acquire("purge").await.unwrap().is_some());

        leader.depose();
        assert!(!guard.is_held().await);
    }

    #[tokio::test]
    async fn a_presigned_upload_is_not_an_uploaded_object() {
        let store = InMemoryAttachments::new();
        let signed = store
            .presign_upload(&UploadRequest {
                content_type: "image/png".to_owned(),
                content_length: 1_024,
            })
            .await
            .unwrap();

        assert!(
            !store.exists(signed.id).await.unwrap(),
            "a payload can reference bytes that never arrived"
        );

        store.mark_uploaded(signed.id);
        assert!(store.exists(signed.id).await.unwrap());

        store.delete(signed.id).await.unwrap();
        assert!(!store.exists(signed.id).await.unwrap());
        assert!(store.uploaded().is_empty());
    }
}

// --- Key generation --------------------------------------------------------

/// A key generator that counts instead of rolling dice: `k0000000`, `k0000001`…
#[derive(Debug, Clone, Default)]
pub struct CountingKeyGenerator {
    next: Arc<Mutex<u32>>,
}

impl CountingKeyGenerator {
    /// A generator starting from zero.
    pub fn new() -> Self {
        Self::default()
    }
}

impl KeyGenerator for CountingKeyGenerator {
    fn generate(&self) -> (ApiKeyPrefix, String) {
        let mut next = lock(&self.next);
        let index = *next;
        *next += 1;

        let index = index % 10_000_000;
        let prefix = ApiKeyPrefix::parse(&format!("k{index:07}"))
            .expect("a counter under 10^7 always renders as eight alphanumerics");
        (prefix, format!("test-secret-{index}"))
    }
}

// --- Stream leases --------------------------------------------------------

/// Cluster stream leases in memory: one shared instance stands for two replicas.
#[derive(Debug, Clone, Default)]
pub struct TestStreamLeases {
    held: Arc<Mutex<Vec<(String, String)>>>,
    next: Arc<Mutex<u64>>,
}

impl TestStreamLeases {
    /// No lease held.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many leases are held, all principals together.
    pub fn held(&self) -> usize {
        lock(&self.held).len()
    }
}

#[async_trait::async_trait]
impl StreamLeases for TestStreamLeases {
    async fn acquire(
        &self,
        principal: &str,
        _replica: &str,
        max: usize,
    ) -> PortResult<Option<String>> {
        let mut held = lock(&self.held);
        if held.iter().filter(|(_, p)| p == principal).count() >= max {
            return Ok(None);
        }
        let mut next = lock(&self.next);
        *next += 1;
        let id = next.to_string();
        held.push((id.clone(), principal.to_owned()));
        Ok(Some(id))
    }

    async fn release(&self, lease: &str) -> PortResult<()> {
        lock(&self.held).retain(|(id, _)| id != lease);
        Ok(())
    }

    async fn renew(&self, _replica: &str) -> PortResult<()> {
        Ok(())
    }
}

// --- Rate limiting ---------------------------------------------------------

/// One recorded call: the subject, and the configured ceiling it came with.
pub type QuotaCheck = (Quota, Option<u32>);

/// A rate limiter a test sets the verdict of: allows everything until told otherwise (T14).
#[derive(Debug, Clone, Default)]
pub struct TestRateLimiter {
    limited: Arc<Mutex<Vec<Quota>>>,
    seen: Arc<Mutex<Vec<QuotaCheck>>>,
}

impl TestRateLimiter {
    /// A limiter that allows everything.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start refusing this exact quota subject.
    pub fn limit(&self, quota: Quota) -> &Self {
        lock(&self.limited).push(quota);
        self
    }

    /// Every quota that was checked, in order.
    pub fn seen(&self) -> Vec<Quota> {
        lock(&self.seen)
            .iter()
            .map(|(quota, _)| quota.clone())
            .collect()
    }

    /// Every quota that was checked, with the configured ceiling it was given.
    pub fn seen_with_ceilings(&self) -> Vec<QuotaCheck> {
        lock(&self.seen).clone()
    }
}

#[async_trait::async_trait]
impl RateLimiter for TestRateLimiter {
    async fn check(
        &self,
        quota: &Quota,
        configured: Option<u32>,
        _now: OffsetDateTime,
    ) -> PortResult<Allowance> {
        lock(&self.seen).push((quota.clone(), configured));
        Ok(if lock(&self.limited).contains(quota) {
            Allowance::Limited {
                retry_after_seconds: 60,
            }
        } else {
            Allowance::Allowed { remaining: 59 }
        })
    }
}
