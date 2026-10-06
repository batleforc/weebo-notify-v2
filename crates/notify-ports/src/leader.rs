//! Electing the replica that runs background jobs once per instance (§11).
//! Jobs check [`LeaderGuard::is_held`] between batches to avoid split brain.

use crate::error::PortResult;

/// Proof that this replica currently leads; dropping it releases the lock.
#[async_trait::async_trait]
pub trait LeaderGuard: Send + Sync + core::fmt::Debug {
    /// Whether leadership still holds; check between batches (scenario T22).
    async fn is_held(&self) -> bool;

    /// Release leadership without waiting for a timeout (clean `SIGTERM` drain, §11).
    async fn release(&self);
}

/// Acquiring leadership.
#[async_trait::async_trait]
pub trait LeaderElection: Send + Sync {
    /// Try to become leader for a named job, without blocking. `None` means
    /// somebody else has it.
    async fn try_acquire(&self, job: &'static str) -> PortResult<Option<Box<dyn LeaderGuard>>>;
}
