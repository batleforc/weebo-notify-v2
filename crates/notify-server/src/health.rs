//! Health and readiness surfaces (design §13).
//!
//! `/healthz` answers "is this process alive"; `/readyz` answers "should this
//! replica be in the load balancer" and therefore checks the dependencies a
//! replica genuinely cannot serve without: Postgres, the object store, the JWKS
//! caches — and Redis when the `eventbus-redis` backend is active, since a replica
//! with no fan-out must not declare itself ready.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Liveness answer.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub(crate) struct Health {
    /// Always `"ok"` — the process answered, which is the whole question.
    #[schema(example = "ok")]
    pub status: String,
    /// Version of the running binary, so an operator can tell replicas apart
    /// during a rolling update.
    #[schema(example = "0.1.0")]
    pub version: String,
}

/// Readiness answer: the overall verdict plus the per-dependency detail an
/// operator needs to know *which* dependency is holding the replica back.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub(crate) struct Readiness {
    /// `true` only when every checked dependency is reachable.
    pub ready: bool,
    /// One entry per dependency actually configured on this replica.
    pub checks: Vec<DependencyCheck>,
}

/// The state of a single dependency.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub(crate) struct DependencyCheck {
    /// Dependency name, e.g. `postgres`, `attachments`, `jwks`, `eventbus`.
    #[schema(example = "postgres")]
    pub name: String,
    /// Whether the last probe succeeded.
    pub healthy: bool,
    /// Failure detail, omitted when healthy — never contains credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Liveness probe.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "admin",
    responses((status = 200, description = "The process is alive", body = Health))
)]
#[allow(
    dead_code,
    reason = "the Actix handler lands with the HTTP adapter in M1.7"
)]
pub(crate) fn healthz() {}

/// Readiness probe — gates entry into the service during rolling updates.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "admin",
    responses(
        (status = 200, description = "Every dependency is reachable", body = Readiness),
        (status = 503, description = "At least one dependency is down", body = Readiness),
    )
)]
#[allow(
    dead_code,
    reason = "the Actix handler lands with the HTTP adapter in M1.7"
)]
pub(crate) fn readyz() {}
