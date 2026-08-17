//! Electing the replica that runs the background jobs (design §11).
//!
//! TTL purge, GitOps reconciliation, sink retries and do-not-disturb digests all
//! run **once per instance, not once per replica**. Two replicas purging
//! concurrently is not merely wasteful; two replicas delivering the same webhook is
//! a bug the consumer sees.
//!
//! Two backends, both configuration: a Postgres advisory lock by default (no extra
//! dependency at all) and the Kubernetes Lease API where the cluster already
//! provides one. Losing the lock has to be *observable* rather than merely likely —
//! a replica that believes it is still leader while another one has taken over is
//! precisely the split brain this exists to prevent, so the guard exposes
//! [`LeaderGuard::is_held`] and the jobs check it between batches.

use crate::error::PortResult;

/// Proof that this replica currently leads.
///
/// Dropping it releases the lock, so a job that panics or exits early does not
/// leave the instance without a leader until a lease times out.
#[async_trait::async_trait]
pub trait LeaderGuard: Send + Sync + core::fmt::Debug {
    /// Whether leadership still holds.
    ///
    /// Checked between batches, not once at the top: a purge loop that took the
    /// lock, lost it during a network partition and kept deleting is the failure
    /// mode worth engineering against (scenario T22).
    async fn is_held(&self) -> bool;

    /// Release leadership explicitly, so another replica can pick it up without
    /// waiting for a timeout — what a clean `SIGTERM` drain should do (design §11).
    async fn release(&self);
}

/// Acquiring leadership.
#[async_trait::async_trait]
pub trait LeaderElection: Send + Sync {
    /// Try to become leader for a named job.
    ///
    /// Non-blocking on purpose: a replica that is not the leader must go on serving
    /// the API immediately rather than parking a task on a lock it may never get.
    /// `None` means somebody else has it, which is a normal answer.
    async fn try_acquire(&self, job: &'static str) -> PortResult<Option<Box<dyn LeaderGuard>>>;
}
