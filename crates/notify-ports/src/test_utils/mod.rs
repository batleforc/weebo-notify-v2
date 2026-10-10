//! In-memory implementations of every port (§4), held to the real contracts:
//! one shared monotonic cursor sequence, `external_id` races conflict, `ack` dedups,
//! `choice` replaces, device cursors never move back. Clones of [`InMemory`] share state.

#![allow(
    clippy::must_use_candidate,
    reason = "test doubles are read in the test that uses them; #[must_use] on every \
              accessor is noise here"
)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "this module is feature-gated and never linked into the server; a panic \
              here surfaces as a failed test, which is exactly what a double is for"
)]

pub mod clock;
pub mod infra;
pub mod store;

pub use clock::TestClock;
pub use infra::{
    CountingKeyGenerator, InMemoryAttachments, InMemoryReplayGuard, RecordingSinkDispatcher,
    StaticFederationVerifier, StaticSinkRegistry, StaticTokenVerifier, StubSecretHasher,
    TestLeaderElection, TestRateLimiter, TestStreamLeases, test_fingerprint_key,
};
pub use store::InMemory;
