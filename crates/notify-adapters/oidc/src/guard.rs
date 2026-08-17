//! Bounding what an unauthenticated caller can make the KDF spend (design §5.2).
//!
//! [`Argon2SecretHasher`](crate::Argon2SecretHasher) is deliberately expensive —
//! 19 MiB and two passes per call — because that is what makes a stolen database of
//! hashes survivable. That argument is about an **offline** attacker with the hashes
//! in hand. It says nothing about the online cost, and the online cost was being
//! paid on every single API-key request, with no cache and with rate limiting
//! sitting *inside* the use cases, which is to say after authentication.
//!
//! The shape of the problem: the prefix half of `wnk_<prefix>_<secret>` is not a
//! secret. It is printed at mint, returned by `GET /keys`, and it is the half people
//! leave visible when they redact a key in a ticket or a screenshot. Anyone holding
//! one could send garbage secrets against it and force a full Argon2 evaluation per
//! request, before any limiter ran — a few hundred concurrent connections is enough
//! to exhaust the blocking pool and push a replica past its memory limit, taking
//! every live stream on it down too.
//!
//! # Two halves, and why neither alone is enough
//!
//! **Memoisation** ([`Verdicts`]). `SecretHasher::verify(secret, hash)` is a *pure
//! function*: the same pair always gives the same answer, so remembering it changes
//! no semantics whatsoever. This is deliberately a cache of the KDF verdict and
//! **not** of the authenticated caller — expiry, revocation and service-account
//! deactivation are still read from the database on every request, so a revoked key
//! stops working immediately rather than at the end of some TTL. It kills the cheap
//! version of the attack (the same wrong secret, repeatedly) and, incidentally, the
//! throughput ceiling for a CI fleet publishing through one key.
//!
//! **Admission control** ([`Gate`]). Memoisation does nothing against an attacker
//! who varies the secret every time, because every guess is a genuine cache miss.
//! What bounds that is refusing to have more than a handful of evaluations in flight
//! at once, and shedding rather than queueing without limit once the backlog is
//! plainly not a burst. Concurrency `× 19 MiB` is then the ceiling on what this
//! costs, whatever arrives.
//!
//! # Why a semaphore rather than `max_blocking_threads`
//!
//! The obvious knob is Tokio's blocking pool, whose default of 512 threads is where
//! the 9.7 GB figure comes from. It is not reachable here: `actix-web` gives each
//! worker its own `actix-rt` Arbiter with its own current-thread runtime and its own
//! blocking pool, so there is no single pool to cap — `System::with_tokio_rt` sets
//! the *system* runtime's, not the workers'. A semaphore in front of the KDF bounds
//! the same resource without depending on runtime internals, and bounds it more
//! tightly: it counts the calls we actually care about instead of every blocking
//! task in the process.
//!
//! # What this is not
//!
//! It is not a rate limiter, and it is deliberately not keyed on the client address.
//! Behind an ingress the peer address is the ingress, and the forwarded one is a
//! header an attacker sets — trusting it without a configured proxy list is how a
//! limiter becomes a way to frame somebody else. Per-address limiting belongs at the
//! ingress, where the address is a fact rather than a claim; the chart's production
//! example and `hardening.mdx` say so.

use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lru::LruCache;
use notify_ports::{PortError, PortResult, SecretHasher};
use sha2::{Digest as _, Sha256};
use tokio::sync::Semaphore;

/// The label overload is reported under, matching `secrets.rs`.
const WHAT: &str = "hasher";

/// How many verified `(secret, hash)` pairs to remember.
///
/// Small: it holds one entry per key actually in use, and an instance with a
/// thousand live API keys is a large one.
const VERIFIED_CAPACITY: usize = 1_024;

/// How many rejected pairs to remember.
///
/// Larger than the verified cache and, more to the point, **separate** from it. One
/// cache would let an attacker walk a thousand wrong secrets through it and evict
/// every legitimate entry, putting the KDF back on the hot path for everybody —
/// paying for the attack twice. Sized so a flood is absorbed rather than merely
/// observed.
const REJECTED_CAPACITY: usize = 16_384;

/// How deep the queue in front of the KDF may get before calls are shed.
///
/// Per permit, so the real ceiling scales with the machine. A legitimate burst is
/// absorbed — and after the first request per key it does not reach the KDF at all,
/// because the verdict is cached — while a flood of novel secrets is refused at a
/// constant cost instead of being queued into a latency collapse.
const QUEUE_DEPTH_PER_PERMIT: usize = 8;

/// A `SecretHasher` that makes the expensive one cheap to repeat and impossible to
/// flood.
///
/// Wraps any other, so the test suite's stub stays exactly as cheap as it was and
/// this type is what production wires around [`crate::Argon2SecretHasher`].
#[derive(Debug)]
pub struct GuardedSecretHasher<H> {
    inner: H,
    verdicts: Verdicts,
    gate: Gate,
}

impl<H> GuardedSecretHasher<H> {
    /// Wrap `inner`, sizing the gate from the machine's parallelism.
    ///
    /// One evaluation in flight per core: the KDF is CPU-bound, so more than that
    /// buys no throughput and costs 19 MiB apiece.
    #[must_use]
    pub fn new(inner: H) -> Self {
        let permits = std::thread::available_parallelism().map_or(4, NonZeroUsize::get);
        Self::with_permits(inner, permits)
    }

    /// Wrap `inner` with an explicit concurrency, for tests that need to provoke the
    /// shedding path without spawning a core's worth of work.
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
    /// Straight through. Hashing happens when a key is *minted*, which is an
    /// authenticated, deliberate, once-per-key act — there is nothing to memoise
    /// (every call salts differently, so no two results are equal) and nothing an
    /// anonymous caller can drive.
    async fn hash(&self, secret: &str) -> PortResult<String> {
        self.inner.hash(secret).await
    }

    async fn verify(&self, secret: &str, hash: &str) -> PortResult<bool> {
        let key = Verdicts::key(secret, hash);

        if let Some(remembered) = self.verdicts.get(&key) {
            return Ok(remembered);
        }

        // Held across the call, released on drop — including on the error path, which
        // is the half that matters: a KDF that starts failing must not leak permits
        // until nothing can authenticate at all.
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

    /// Digest the pair rather than keying on it directly.
    ///
    /// The presented secret is a live credential; a cache keyed on the plaintext
    /// would keep thousands of them in memory for as long as the process runs, ready
    /// to be read out of a core dump. SHA-256 is the right primitive precisely
    /// because it is *fast* — the expensive KDF is what this cache exists to avoid,
    /// and its cost buys nothing here: an attacker who could invert this already
    /// holds the secret.
    ///
    /// The stored hash goes in too, so a rotated secret can never inherit the
    /// verdict of the one it replaced, and the two fields are length-prefixed so no
    /// pair can be made to digest as another.
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
        // `get` promotes the entry, which is the point of an LRU: the keys in active
        // use stay, and a flood of one-shot guesses ages itself out.
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
        // A poisoned lock means another thread panicked mid-update. Losing the
        // memoisation is survivable — the next call simply pays the KDF — whereas
        // propagating it would turn one panic into an instance that cannot
        // authenticate anybody.
        if let Ok(mut cache) = cache.lock() {
            cache.put(key, ());
        }
    }
}

/// The bounded doorway in front of the KDF.
#[derive(Debug)]
struct Gate {
    permits: Semaphore,
    /// In flight plus waiting. Tracked separately because a semaphore's queue is not
    /// observable, and shedding needs to know how deep it already is.
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

    /// Claim a place, or refuse.
    ///
    /// Refusing is the *point*: an unbounded wait converts a memory problem into a
    /// latency problem and calls it a fix, while every waiting request still holds a
    /// connection and a task. `Unavailable` surfaces as a `503`, which is what this
    /// is — the process shedding load it has decided not to spend.
    ///
    /// The counter is incremented *before* the wait, which is what makes the depth
    /// reading mean "already asking" rather than "already running".
    async fn enter(&self) -> PortResult<Admitted<'_>> {
        let depth = self.pending.fetch_add(1, Ordering::Relaxed);
        // From here every exit is a drop, so the increment above is undone exactly
        // once on all three paths: shed, failed acquire, and the happy one.
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
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

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
        // Otherwise the attack pays twice: once for its own guesses, and again by
        // putting every legitimate key back on the KDF.
        let hasher = GuardedSecretHasher::new(Counting::default());
        assert!(hasher.verify("right", "$stored$").await.unwrap());

        for attempt in 0..(VERIFIED_CAPACITY * 2) {
            let _ = hasher.verify(&format!("guess-{attempt}"), "$stored$").await;
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

        // Same secret, different stored hash: a different question, so it must be
        // asked rather than answered from memory.
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

        // One permit, so the ceiling is QUEUE_DEPTH_PER_PERMIT. Fill it with calls
        // that cannot finish until the semaphore is posted.
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
            let _ = task.await;
        }

        // And the counter unwinds, so shedding is not a one-way door.
        assert_eq!(hasher.gate.pending.load(Ordering::Relaxed), 0);
        assert!(hasher.verify("right", "$stored$").await.is_ok());
    }

    #[tokio::test]
    async fn minting_is_never_memoised() {
        // Two calls, two different salts, so a cache here would be wrong as well as
        // useless — and nothing anonymous can reach it anyway.
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
