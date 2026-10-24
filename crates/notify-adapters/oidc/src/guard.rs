//! Bounds what an unauthenticated caller can make the Argon2 KDF spend (design §5.2).
//! Key prefixes are public, so garbage secrets could force a 19 MiB KDF per request. Two defences:
//! [`Verdicts`] memoises the pure `verify(secret, hash)` (not the caller: revocation still hits the DB),
//! and [`Gate`] bounds in-flight evaluations with a semaphore (actix has per-worker blocking pools),
//! shedding past a queue depth. Not a rate limiter; per-address limiting belongs at the ingress.

use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lru::LruCache;
use notify_ports::{PortError, PortResult, SecretHasher};
use sha2::{Digest as _, Sha256};
use tokio::sync::Semaphore;

/// The label overload is reported under, matching `secrets.rs`.
const WHAT: &str = "hasher";

/// How many verified `(secret, hash)` pairs to remember: one per key in use.
const VERIFIED_CAPACITY: usize = 1_024;

/// How many rejected pairs to remember. Separate from the verified cache so a flood of
/// wrong secrets cannot evict legitimate entries.
const REJECTED_CAPACITY: usize = 16_384;

/// Max queue depth per permit before shedding: bursts are absorbed, floods refused at constant cost.
const QUEUE_DEPTH_PER_PERMIT: usize = 8;

/// A `SecretHasher` wrapper that memoises verdicts and bounds KDF concurrency; production wraps Argon2.
#[derive(Debug)]
pub struct GuardedSecretHasher<H> {
    inner: H,
    verdicts: Verdicts,
    gate: Gate,
}

impl<H> GuardedSecretHasher<H> {
    /// Wrap `inner`, one evaluation in flight per core (CPU-bound, 19 MiB apiece).
    #[must_use]
    pub fn new(inner: H) -> Self {
        let permits = std::thread::available_parallelism().map_or(4, NonZeroUsize::get);
        Self::with_permits(inner, permits)
    }

    /// Wrap `inner` with an explicit concurrency, for tests of the shedding path.
    #[must_use]
    pub fn with_permits(inner: H, permits: usize) -> Self {
        let permits = permits.max(1);
        Self {
            inner,
            verdicts: Verdicts::new(),
            gate: Gate::new(permits),
        }
    }
}

#[async_trait::async_trait]
impl<H: SecretHasher> SecretHasher for GuardedSecretHasher<H> {
    /// Straight through: minting is authenticated and salted, so nothing to memoise.
    async fn hash(&self, secret: &str) -> PortResult<String> {
        self.inner.hash(secret).await
    }

    async fn verify(&self, secret: &str, hash: &str) -> PortResult<bool> {
        let key = Verdicts::key(secret, hash);

        if let Some(remembered) = self.verdicts.get(&key) {
            return Ok(remembered);
        }

        // Released on drop, including on error, so a failing KDF cannot leak permits.
        let _admitted = self.gate.enter().await?;

        let verdict = self.inner.verify(secret, hash).await?;
        self.verdicts.remember(key, verdict);
        Ok(verdict)
    }
}

/// The memoised verdicts, positives and negatives kept apart.
#[derive(Debug)]
struct Verdicts {
    verified: Mutex<LruCache<[u8; 32], ()>>,
    rejected: Mutex<LruCache<[u8; 32], ()>>,
}

impl Verdicts {
    fn new() -> Self {
        Self {
            verified: Mutex::new(LruCache::new(
                NonZeroUsize::new(VERIFIED_CAPACITY).unwrap_or(NonZeroUsize::MIN),
            )),
            rejected: Mutex::new(LruCache::new(
                NonZeroUsize::new(REJECTED_CAPACITY).unwrap_or(NonZeroUsize::MIN),
            )),
        }
    }

    /// Digest the pair: no plaintext credentials in memory. SHA-256 is fine since inverting it
    /// already requires the secret. The stored hash is included (length-prefixed) so rotation never inherits.
    fn key(secret: &str, hash: &str) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(
            u64::try_from(secret.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        digest.update(secret.as_bytes());
        digest.update(u64::try_from(hash.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(hash.as_bytes());
        digest.finalize().into()
    }

    fn get(&self, key: &[u8; 32]) -> Option<bool> {
        // `get` promotes: active keys stay, one-shot guesses age out.
        if let Ok(mut verified) = self.verified.lock()
            && verified.get(key).is_some()
        {
            return Some(true);
        }
        if let Ok(mut rejected) = self.rejected.lock()
            && rejected.get(key).is_some()
        {
            return Some(false);
        }
        None
    }

    fn remember(&self, key: [u8; 32], verdict: bool) {
        let cache = if verdict {
            &self.verified
        } else {
            &self.rejected
        };
        // Survive a poisoned lock: losing memoisation beats being unable to authenticate anyone.
        if let Ok(mut cache) = cache.lock() {
            cache.put(key, ());
        }
    }
}

/// The bounded doorway in front of the KDF.
#[derive(Debug)]
struct Gate {
    permits: Semaphore,
    /// In flight plus waiting; a semaphore's queue is not observable.
    pending: AtomicUsize,
    ceiling: usize,
}

impl Gate {
    fn new(permits: usize) -> Self {
        Self {
            permits: Semaphore::new(permits),
            pending: AtomicUsize::new(0),
            ceiling: permits.saturating_mul(QUEUE_DEPTH_PER_PERMIT),
        }
    }

    /// Claim a place, or refuse with `Unavailable` (a `503`) rather than wait unbounded.
    /// The counter is incremented before the wait, so depth means "already asking".
    async fn enter(&self) -> PortResult<Admitted<'_>> {
        let depth = self.pending.fetch_add(1, Ordering::Relaxed);
        // Every exit from here is a drop, undoing the increment exactly once.
        let pending = Pending {
            counter: &self.pending,
        };

        if depth >= self.ceiling {
            tracing::warn!(
                depth,
                ceiling = self.ceiling,
                "shedding a credential verification: the KDF queue is past its ceiling"
            );
            return Err(PortError::unavailable(WHAT, Overloaded));
        }

        let permit = self
            .permits
            .acquire()
            .await
            .map_err(|error| PortError::unavailable(WHAT, error))?;

        Ok(Admitted {
            _pending: pending,
            _permit: permit,
        })
    }
}

/// Decrements the pending counter on drop, whatever the exit path.
#[derive(Debug)]
struct Pending<'a> {
    counter: &'a AtomicUsize,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Holds both a place in the queue and a permit, for as long as it lives.
#[derive(Debug)]
struct Admitted<'a> {
    _pending: Pending<'a>,
    _permit: tokio::sync::SemaphorePermit<'a>,
}

/// What a shed verification reports as its cause.
#[derive(Debug)]
struct Overloaded;

impl core::fmt::Display for Overloaded {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "too many credential verifications are already in flight; \
             this one was shed rather than queued"
        )
    }
}

impl core::error::Error for Overloaded {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Counts how often the real work ran, so a test can prove it did not.
    #[derive(Debug, Default)]
    struct Counting {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl SecretHasher for Counting {
        async fn hash(&self, _secret: &str) -> PortResult<String> {
            Ok("$stub$".to_owned())
        }

        async fn verify(&self, secret: &str, hash: &str) -> PortResult<bool> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(secret == "right" && hash == "$stored$")
        }
    }

    /// Blocks until released, so the gate's queue can be filled deterministically.
    #[derive(Debug)]
    struct Blocking {
        release: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait::async_trait]
    impl SecretHasher for Blocking {
        async fn hash(&self, _secret: &str) -> PortResult<String> {
            Ok(String::new())
        }

        async fn verify(&self, _secret: &str, _hash: &str) -> PortResult<bool> {
            let _held = self.release.acquire().await;
            Ok(false)
        }
    }

    #[tokio::test]
    async fn a_repeated_correct_secret_costs_one_evaluation() {
        let hasher = GuardedSecretHasher::new(Counting::default());

        for _ in 0..50 {
            assert!(hasher.verify("right", "$stored$").await.unwrap());
        }

        assert_eq!(
            hasher.inner.calls.load(Ordering::Relaxed),
            1,
            "a CI fleet publishing through one key must not pay the KDF per request"
        );
    }

    #[tokio::test]
    async fn a_repeated_wrong_secret_costs_one_evaluation() {
        // The attack, in its cheap form: the same guess, thousands of times.
        let hasher = GuardedSecretHasher::new(Counting::default());

        for _ in 0..50 {
            assert!(!hasher.verify("wrong", "$stored$").await.unwrap());
        }

        assert_eq!(hasher.inner.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_flood_of_wrong_secrets_cannot_evict_the_verified_ones() {
        // Otherwise a flood would put every legitimate key back on the KDF.
        let hasher = GuardedSecretHasher::new(Counting::default());
        assert!(hasher.verify("right", "$stored$").await.unwrap());

        for attempt in 0..(VERIFIED_CAPACITY * 2) {
            assert!(
                !hasher
                    .verify(&format!("guess-{attempt}"), "$stored$")
                    .await
                    .unwrap()
            );
        }

        let before = hasher.inner.calls.load(Ordering::Relaxed);
        assert!(hasher.verify("right", "$stored$").await.unwrap());
        assert_eq!(
            hasher.inner.calls.load(Ordering::Relaxed),
            before,
            "the good verdict was evicted by the flood"
        );
    }

    #[tokio::test]
    async fn a_rotated_secret_does_not_inherit_the_old_verdict() {
        let hasher = GuardedSecretHasher::new(Counting::default());
        assert!(hasher.verify("right", "$stored$").await.unwrap());

        // Different stored hash: a different question.
        assert!(!hasher.verify("right", "$rotated$").await.unwrap());
        assert_eq!(hasher.inner.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn verifications_past_the_ceiling_are_shed_rather_than_queued() {
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let hasher = Arc::new(GuardedSecretHasher::with_permits(
            Blocking {
                release: Arc::clone(&release),
            },
            1,
        ));

        // One permit, so the ceiling is QUEUE_DEPTH_PER_PERMIT; fill it with blocked calls.
        let mut held = Vec::new();
        for attempt in 0..=QUEUE_DEPTH_PER_PERMIT {
            let hasher = Arc::clone(&hasher);
            held.push(tokio::spawn(async move {
                hasher
                    .verify(&format!("secret-{attempt}"), "$stored$")
                    .await
            }));
        }

        // Let them all reach the gate before asking for one more.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let shed = hasher.verify("one-too-many", "$stored$").await;
        assert!(
            matches!(shed, Err(PortError::Unavailable { .. })),
            "past the ceiling the answer is a refusal, not an unbounded wait: {shed:?}"
        );

        release.add_permits(held.len() + 1);
        for task in held {
            let verdict = task.await.unwrap();
            // Admitted calls answer once released; the rest were shed at the gate.
            assert!(
                matches!(verdict, Ok(false) | Err(PortError::Unavailable { .. })),
                "a held call ends in an answer or a refusal: {verdict:?}"
            );
        }

        // And the counter unwinds, so shedding is not a one-way door.
        assert_eq!(hasher.gate.pending.load(Ordering::Relaxed), 0);
        assert!(hasher.verify("right", "$stored$").await.is_ok());
    }

    #[tokio::test]
    async fn minting_is_never_memoised() {
        // Different salts each time: a cache here would be wrong.
        let hasher = GuardedSecretHasher::new(Counting::default());

        assert_eq!(hasher.hash("secret").await.unwrap(), "$stub$");
        assert_eq!(hasher.hash("secret").await.unwrap(), "$stub$");
        assert_eq!(
            hasher.inner.calls.load(Ordering::Relaxed),
            0,
            "hashing must not go through the verify cache"
        );
    }
}
