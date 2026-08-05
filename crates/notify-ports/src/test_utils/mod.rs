//! In-memory implementations of every port (design §4).
//!
//! This is what makes the promise in the architecture doc real: `notify-domain` and
//! `notify-app` are testable with no container, no network and no clock, which is
//! why a ≥ 90 % coverage gate on those two crates is a reasonable thing to ask for
//! (design §3.2). A use-case test that needs Postgres is a use-case test that gets
//! skipped.
//!
//! # These doubles are held to the real contract
//!
//! A double that is merely *convenient* is worse than no double: it makes the layer
//! above pass tests that the production adapter will fail. So the ones here
//! reproduce the behaviour the trait documentation promises, including the parts
//! that are inconvenient —
//!
//! * cursors are allocated from one shared monotonic sequence, so ordering across
//!   repositories is coherent (ADR-0012);
//! * an upsert racing on the same `external_id` conflicts;
//! * `ack` deduplicates and `choice` replaces;
//! * a device cursor never moves backwards;
//! * expired-undelivered notifications are counted, not backlogged (design §7.2bis).
//!
//! They are exercised by this crate's own tests for exactly that reason.
//!
//! # Shape
//!
//! [`InMemory`] holds every repository behind one shared state, so a test wires the
//! same handle into every port and the whole thing stays consistent. Cloning it is
//! cheap and shares that state — which is the point:
//!
//! ```
//! # use notify_ports::test_utils::InMemory;
//! # use std::sync::Arc;
//! let store = InMemory::new();
//! let notifications = Arc::new(store.clone());
//! let channels = Arc::new(store.clone());
//! // …both see the same data.
//! ```
//!
//! The stateless-ish infrastructure ports get their own small types in [`infra`],
//! since sharing state with a notification store would buy nothing.

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
    StaticFederationVerifier, StaticTokenVerifier, StubSecretHasher, TestLeaderElection,
    TestRateLimiter,
};
pub use store::InMemory;
