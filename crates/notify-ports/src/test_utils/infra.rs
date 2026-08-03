//! Doubles for the infrastructure ports — the ones with no notification state to
//! share.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use notify_domain::{ApiKeyPrefix, AttachmentId, ChannelId, Scope};
use time::{Duration, OffsetDateTime};
use url::Url;

use crate::attachments::{
    AttachmentStore, PresignedDownload, PresignedUpload, StoredObject, UploadRequest,
};
use crate::clock::Clock;
use crate::error::{PortError, PortResult};
use crate::identities::{KeyGenerator, SecretHasher};
use crate::leader::{LeaderElection, LeaderGuard};
use crate::quota::{Allowance, Quota, RateLimiter};
use crate::sinks::{SinkDelivery, SinkDispatcher, SinkOutcome};
use crate::tokens::{MintedToken, TokenRejection, TokenVerifier, VerifiedClaims};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock in a test means an earlier assertion already failed. Reading
    // through it gives that failure a chance to be the one reported, instead of
    // burying it under a panic from every other port in the fixture.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- Secret hashing --------------------------------------------------------

/// A hasher that is not one.
///
/// Argon2id is deliberately expensive; a suite that authenticates a few hundred
/// times would spend its whole runtime in a KDF. The usual "fix" — turning the
/// production cost parameters down until the tests are bearable — weakens the real
/// thing to speed up the fake one, which is why this port exists at all.
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

/// A verifier with a fixed table of tokens.
///
/// Anything not in the table is [`TokenRejection::BadSignature`], so a test that
/// forgets to register a token fails as an unauthenticated request rather than
/// silently authenticating as somebody.
#[derive(Debug, Clone)]
pub struct StaticTokenVerifier {
    tokens: Arc<Mutex<Vec<(String, VerifiedClaims)>>>,
    healthy: Arc<Mutex<bool>>,
    clock: Arc<dyn Clock>,
}

impl StaticTokenVerifier {
    /// An empty verifier reading the real clock: every token is refused.
    pub fn new() -> Self {
        Self::with_clock(Arc::new(crate::clock::SystemClock))
    }

    /// An empty verifier reading the clock a test controls.
    ///
    /// Expiry is checked against it, so "this token is stale" is a statement the
    /// test can make rather than one that depends on what year it is run in.
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            tokens: Arc::new(Mutex::new(Vec::new())),
            healthy: Arc::new(Mutex::new(true)),
            clock,
        }
    }

    /// Register a token and the claims it stands for.
    pub fn register(&self, token: &str, claims: VerifiedClaims) -> &Self {
        lock(&self.tokens).push((token.to_owned(), claims));
        self
    }

    /// Make the JWKS cache look cold, so `/readyz` behaviour can be tested.
    pub fn set_healthy(&self, healthy: bool) {
        *lock(&self.healthy) = healthy;
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
            // Expiry is checked against the claims rather than assumed valid: an
            // expired token in the table must still be refused.
            Some(claims) if claims.expires_at <= self.clock.now() => Err(TokenRejection::Expired),
            Some(claims) => Ok(claims),
            None => Err(TokenRejection::BadSignature),
        })
    }

    async fn health(&self) -> PortResult<()> {
        if *lock(&self.healthy) {
            Ok(())
        } else {
            Err(PortError::NotConfigured { what: "jwks" })
        }
    }
}

/// Build a minted CI token without a signer, for tests that only care about what a
/// policy resolved to (design §5.3).
pub fn minted_token(
    channels: Vec<ChannelId>,
    scopes: Vec<Scope>,
    now: OffsetDateTime,
) -> MintedToken {
    MintedToken {
        token: "test-ci-token".to_owned(),
        expires_at: now + Duration::minutes(15),
        channels,
        scopes,
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

    /// This replica wins none — the case worth testing, because "nothing happened"
    /// is a much easier bug to miss than "it happened twice".
    pub fn follower() -> Self {
        Self {
            leader: Arc::new(Mutex::new(false)),
        }
    }

    /// Take leadership away mid-run, which is what a network partition does.
    /// A job already holding a guard must notice through
    /// [`LeaderGuard::is_held`] and stop between batches (scenario T22).
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
        *lock(&self.leader) = false;
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

/// An object store that signs URLs nobody can upload to, and remembers what it was
/// asked for.
#[derive(Debug, Clone, Default)]
pub struct InMemoryAttachments {
    uploaded: Arc<Mutex<Vec<AttachmentId>>>,
    /// Bytes the server wrote itself — the avatar path (design §2.4).
    objects: Arc<Mutex<std::collections::HashMap<AttachmentId, StoredObject>>>,
}

impl InMemoryAttachments {
    /// An empty bucket.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pretend an upload completed, so [`AttachmentStore::exists`] says yes.
    ///
    /// The gap between "presigned" and "uploaded" is a real state a payload can
    /// reference, so it is one the double can reproduce.
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
            headers: vec![
                ("content-type".to_owned(), request.content_type.clone()),
                (
                    "content-length".to_owned(),
                    request.content_length.to_string(),
                ),
            ],
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

    async fn health(&self) -> PortResult<()> {
        Ok(())
    }
}

// --- Sinks -----------------------------------------------------------------

/// A sink dispatcher that records what it was handed and answers however the test
/// tells it to.
#[derive(Debug, Clone, Default)]
pub struct RecordingSinkDispatcher {
    delivered: Arc<Mutex<Vec<SinkDelivery>>>,
    outcome: Arc<Mutex<Option<SinkOutcome>>>,
}

impl RecordingSinkDispatcher {
    /// A dispatcher that accepts everything.
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every subsequent delivery come back with this outcome — how a retry,
    /// a backoff and a dead letter get tested without a webserver (scenario T17).
    pub fn always(&self, outcome: SinkOutcome) -> &Self {
        *lock(&self.outcome) = Some(outcome);
        self
    }

    /// Everything it was asked to deliver, in order.
    pub fn delivered(&self) -> Vec<SinkDelivery> {
        lock(&self.delivered).clone()
    }
}

#[async_trait::async_trait]
impl SinkDispatcher for RecordingSinkDispatcher {
    fn handles(&self) -> &'static [&'static str] {
        &["webhook", "email"]
    }

    async fn deliver(&self, delivery: &SinkDelivery) -> PortResult<SinkOutcome> {
        lock(&self.delivered).push(delivery.clone());
        Ok(lock(&self.outcome)
            .clone()
            .unwrap_or(SinkOutcome::Delivered))
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

        // A partition takes the lock away mid-run; a job checking between batches
        // has to see it (scenario T22).
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

/// A key generator that counts instead of rolling dice.
///
/// Randomness is exactly what a test cannot assert against, so this hands out
/// `k0000000`, `k0000001`… and a matching secret. Every key it makes is worthless,
/// which is the point.
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

        // Wraps rather than overflowing the eight-character prefix; a test that
        // mints ten million keys has other problems.
        let index = index % 10_000_000;
        let prefix = ApiKeyPrefix::parse(&format!("k{index:07}"))
            .expect("a counter under 10^7 always renders as eight alphanumerics");
        (prefix, format!("test-secret-{index}"))
    }
}

// --- Rate limiting ---------------------------------------------------------

/// A rate limiter a test sets the verdict of.
///
/// Fixed-window counting would make every quota test a question of what time it is;
/// this one simply allows everything until told otherwise, and then refuses
/// everything. Enough to assert that a `429` is produced and that the *other*
/// publishers of a channel are untouched (scenario T14).
#[derive(Debug, Clone, Default)]
pub struct TestRateLimiter {
    limited: Arc<Mutex<Vec<Quota>>>,
    seen: Arc<Mutex<Vec<Quota>>>,
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
        lock(&self.seen).clone()
    }
}

#[async_trait::async_trait]
impl RateLimiter for TestRateLimiter {
    async fn check(&self, quota: &Quota, _now: OffsetDateTime) -> PortResult<Allowance> {
        lock(&self.seen).push(quota.clone());
        Ok(if lock(&self.limited).contains(quota) {
            Allowance::Limited {
                retry_after_seconds: 60,
            }
        } else {
            Allowance::Allowed { remaining: 59 }
        })
    }
}
