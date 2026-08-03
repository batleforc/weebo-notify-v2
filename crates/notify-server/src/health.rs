//! Health and readiness surfaces (design §13).
//!
//! `/healthz` answers "is this process alive"; `/readyz` answers "should this
//! replica be in the load balancer" and therefore probes the dependencies a replica
//! genuinely cannot serve without.
//!
//! The distinction is the whole point of having two. A replica whose database has
//! gone should leave the pool without being restarted — restarting it will not
//! bring Postgres back, and a restart loop during an outage turns one problem into
//! two. So `/healthz` answers `200` for as long as the process is running, and only
//! `/readyz` reports the dependency.

use actix_web::{HttpResponse, Responder, web};
use notify_persistence_pg::PgStore;
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

/// What this replica probes.
#[derive(Debug, Clone)]
pub(crate) struct Dependencies {
    store: PgStore,
    transports: Vec<&'static str>,
}

impl Dependencies {
    pub(crate) const fn new(store: PgStore, transports: Vec<&'static str>) -> Self {
        Self { store, transports }
    }

    async fn probe(&self) -> Readiness {
        let mut checks = Vec::new();

        // Postgres is both the database and the event bus (ADR-0008), so one probe
        // answers for both — and reporting them separately would suggest an
        // independence this deployment does not have.
        let database = self.store.ping().await;
        checks.push(DependencyCheck {
            name: "postgres".to_owned(),
            healthy: database.is_ok(),
            detail: database.err().map(|error| error.to_string()),
        });

        // Push transports are reported but never gate readiness: a replica with no
        // Web Push key still serves the API, the stream and the inbox perfectly
        // well. In M1 there are none, and that is not a degraded state.
        for transport in &self.transports {
            checks.push(DependencyCheck {
                name: (*transport).to_owned(),
                healthy: true,
                detail: None,
            });
        }

        Readiness {
            ready: checks.iter().all(|check| check.healthy),
            checks,
        }
    }
}

/// Mount both probes.
pub(crate) fn configure(config: &mut web::ServiceConfig) {
    config
        .route("/healthz", web::get().to(healthz))
        .route("/readyz", web::get().to(readyz));
}

/// Liveness probe.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "admin",
    responses((status = 200, description = "The process is alive", body = Health))
)]
async fn healthz() -> impl Responder {
    HttpResponse::Ok().json(Health {
        status: "ok".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

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
async fn readyz(dependencies: web::Data<Dependencies>) -> impl Responder {
    let readiness = dependencies.probe().await;

    // 503 rather than 200-with-a-flag: a load balancer reads the status line, not
    // the body, and a replica that cannot serve must leave the pool.
    if readiness.ready {
        HttpResponse::Ok().json(readiness)
    } else {
        HttpResponse::ServiceUnavailable().json(readiness)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing assertion is a failing test")]

    use actix_web::{App, test};

    use super::*;

    #[actix_web::test]
    async fn liveness_answers_without_touching_a_dependency() {
        // The point of the split: this must answer during a database outage, or a
        // restart loop turns one problem into two.
        let app = test::init_service(App::new().configure(configure)).await;

        let health: Health = test::call_and_read_body_json(
            &app,
            test::TestRequest::get().uri("/healthz").to_request(),
        )
        .await;

        assert_eq!(health.status, "ok");
        assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
    }

    #[actix_web::test]
    async fn readiness_needs_a_database_and_says_which_one_is_missing() {
        // A lazy pool pointed at nothing, with a short acquire timeout: the test is
        // about what an unreachable dependency *reports*, not about waiting for the
        // production timeout to elapse.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();

        let readiness = Dependencies::new(PgStore::new(pool), Vec::new())
            .probe()
            .await;

        assert!(
            !readiness.ready,
            "a replica that cannot serve must leave the pool"
        );
        assert_eq!(readiness.checks[0].name, "postgres");
        assert!(
            readiness.checks[0].detail.is_some(),
            "an operator needs to be told which dependency, not just that one failed"
        );
    }

    #[actix_web::test]
    async fn a_push_transport_is_reported_but_never_gates_readiness() {
        // M1 has no transports at all, and that is not a degraded state: the API,
        // the stream and the inbox all work without one.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();

        let checks = Dependencies::new(PgStore::new(pool), vec!["webpush"])
            .probe()
            .await
            .checks;

        assert!(
            checks
                .iter()
                .any(|check| check.name == "webpush" && check.healthy)
        );
    }
}
