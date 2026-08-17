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

/// Mount the probes and the exporter.
pub(crate) fn configure(config: &mut web::ServiceConfig) {
    config
        .route("/healthz", web::get().to(healthz))
        .route("/readyz", web::get().to(readyz))
        .route("/metrics", web::get().to(metrics));
}

/// The Prometheus exporter (design §13).
///
/// # Why it sits beside the probes rather than under `/api/v1`
///
/// It is scraped by infrastructure, not called by a client: it has no OpenAPI entry
/// (nothing generated from the spec would ever call it), it answers a text format
/// rather than JSON, and it is unauthenticated for the same reason `/healthz` is —
/// a scraper is a pod in the same namespace, not a person with a token.
///
/// That is a real exposure and it is bounded on purpose: the document carries
/// **counts and channel ids**, never a notification, a user, a device or an
/// endpoint. Somebody who can reach this learns how busy the instance is and what
/// its channels are called. An ingress that publishes it is a deployment mistake,
/// and the same one that publishing `/readyz` would be.
async fn metrics(metrics: web::Data<std::sync::Arc<dyn notify_ports::Metrics>>) -> impl Responder {
    HttpResponse::Ok()
        // The version the Prometheus text parser expects; `prometheus-client`
        // renders OpenMetrics, which is a superset it reads.
        .content_type("application/openmetrics-text; version=1.0.0; charset=utf-8")
        .body(metrics.render())
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
    async fn the_exporter_serves_the_registry_the_process_writes_to() {
        // Worth its own test even though the registry is tested where it lives: what
        // can break here is the wiring, and it breaks silently. An `app_data` whose
        // type does not match what the handler extracts is a `500` at scrape time
        // and nothing at all at compile time — so this asserts the number *this*
        // registry recorded comes back out, not merely that the route answers.
        let metrics: std::sync::Arc<dyn notify_ports::Metrics> =
            std::sync::Arc::new(notify_metrics_prometheus::PrometheusMetrics::new());
        metrics.stream_opened(notify_ports::StreamKind::Sse);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(std::sync::Arc::clone(&metrics)))
                .configure(configure),
        )
        .await;

        let response =
            test::call_service(&app, test::TestRequest::get().uri("/metrics").to_request()).await;
        assert!(response.status().is_success());

        // The scraper reads the content type before the body; OpenMetrics is what
        // `prometheus-client` renders and what the Prometheus parser expects.
        let content_type = response
            .headers()
            .get(actix_web::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            content_type.starts_with("application/openmetrics-text"),
            "a scraper reads the content type first: {content_type}"
        );

        let body = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
        assert!(
            body.contains("realtime_connections{transport=\"sse\"} 1"),
            "the endpoint must serve the registry the process counted into: {body}"
        );
    }

    #[actix_web::test]
    async fn an_instance_wired_to_no_exporter_serves_an_empty_document_rather_than_an_error() {
        // The no-op is a configuration answer, not a failure — a deployment that
        // measures nothing should give a scraper an empty scrape, and a `500` here
        // would page somebody for a build that is behaving exactly as configured.
        let metrics: std::sync::Arc<dyn notify_ports::Metrics> =
            std::sync::Arc::new(notify_ports::NoMetrics);
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(metrics))
                .configure(configure),
        )
        .await;

        let response =
            test::call_service(&app, test::TestRequest::get().uri("/metrics").to_request()).await;

        assert!(response.status().is_success());
        assert!(test::read_body(response).await.is_empty());
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
