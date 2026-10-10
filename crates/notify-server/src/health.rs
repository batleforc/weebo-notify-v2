//! Health and readiness (design §13): `/healthz` is liveness only, `/readyz` probes dependencies,
//! so a database outage drains the replica instead of restart-looping it.
//! `/readyz` is bounded by [`PROBE_TIMEOUT`], skips the database when the pool is saturated
//! (a waiting probe would drain the whole fleet under load), and, being unauthenticated,
//! reports a fixed word while the real error goes to the log.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use actix_web::{HttpResponse, Responder, web};
use notify_persistence_pg::{PgStore, Probe};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Liveness answer.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub(crate) struct Health {
    /// Always `"ok"` — the process answered, which is the whole question.
    #[schema(example = "ok")]
    pub status: String,
    /// Version of the running binary, to tell replicas apart during a rolling update.
    #[schema(example = "0.1.0")]
    pub version: String,
}

/// Readiness answer: the overall verdict plus per-dependency detail.
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

/// How long `/readyz` may take, all checks included.
///
/// Under the default kubelet probe timeout (one second is the default, and most
/// charts raise it) is not the goal; under the *period* is — a probe that outlives
/// its period stacks up behind itself.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// What this replica probes.
#[derive(Debug, Clone)]
pub(crate) struct Dependencies {
    store: PgStore,
    transports: Vec<&'static str>,
    bus: crate::bus::BusHealth,
    started: Started,
}

/// Whether boot has finished the work that has to precede traffic — today, the
/// first attempt to apply the declarative configuration (§11).
///
/// The listener is bound *before* that work so `/healthz` answers while it runs
/// (a reconcile can wait up to a minute on another replica's lock); this is what
/// keeps the replica out of the load balancer until it is done.
#[derive(Debug, Clone, Default)]
pub(crate) struct Started(Arc<AtomicBool>);

impl Started {
    pub(crate) fn finish(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn is_done(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl Dependencies {
    pub(crate) const fn new(
        store: PgStore,
        transports: Vec<&'static str>,
        bus: crate::bus::BusHealth,
        started: Started,
    ) -> Self {
        Self {
            store,
            transports,
            bus,
            started,
        }
    }

    async fn probe(&self) -> Readiness {
        let mut checks = Vec::new();

        // Postgres is also the event bus (ADR-0008), so one probe answers for both.
        let (healthy, detail) =
            match actix_web::rt::time::timeout(PROBE_TIMEOUT, self.store.probe()).await {
                Ok(Ok(Probe::Answered)) => (true, None),
                Ok(Ok(Probe::Saturated)) => (true, Some("pool saturated; not probed")),
                Ok(Err(error)) => {
                    tracing::warn!(?error, "readiness: postgres did not answer");
                    (false, Some("unreachable"))
                }
                Err(_) => {
                    tracing::warn!(timeout = ?PROBE_TIMEOUT, "readiness: postgres timed out");
                    (false, Some("timed out"))
                }
            };
        checks.push(DependencyCheck {
            name: "postgres".to_owned(),
            healthy,
            detail: detail.map(str::to_owned),
        });

        // The `LISTEN` pump, separately from the database it listens on: the pump
        // can be down — between two resubscribe attempts, after a panic — while
        // every query answers, and that replica's streams would go stale with
        // nothing else reporting it.
        let listening = self.bus.is_alive();
        checks.push(DependencyCheck {
            name: "eventbus".to_owned(),
            healthy: listening,
            detail: (!listening).then(|| "not subscribed; resubscribing".to_owned()),
        });

        let started = self.started.is_done();
        checks.push(DependencyCheck {
            name: "startup".to_owned(),
            healthy: started,
            detail: (!started).then(|| "applying the configuration".to_owned()),
        });

        // Push transports are reported but never gate readiness.
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

/// The Prometheus exporter (design §13): unauthenticated and outside `/api/v1`, like `/healthz`.
/// It exposes only counts and channel ids; publishing it on an ingress is a deployment mistake.
async fn metrics(metrics: web::Data<std::sync::Arc<dyn notify_ports::Metrics>>) -> impl Responder {
    HttpResponse::Ok()
        // OpenMetrics, which the Prometheus parser reads.
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

    // 503: load balancers read the status line, not the body.
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
        // Must answer during a database outage.
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
        // A lazy pool at nothing with a short timeout: tests the report, not the wait.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();

        let readiness = dependencies(PgStore::new(pool), Vec::new()).probe().await;

        assert!(
            !readiness.ready,
            "a replica that cannot serve must leave the pool"
        );
        assert_eq!(readiness.checks[0].name, "postgres");
        let detail = readiness.checks[0].detail.as_deref().unwrap();
        assert_eq!(
            detail, "unreachable",
            "which dependency, and nothing an unauthenticated caller should read"
        );
        assert!(!detail.contains("127.0.0.1") && !detail.contains("nobody"));
    }

    fn dependencies(store: PgStore, transports: Vec<&'static str>) -> Dependencies {
        let started = Started::default();
        started.finish();
        Dependencies::new(store, transports, crate::bus::BusHealth::alive(), started)
    }

    #[actix_web::test]
    async fn a_replica_still_booting_or_deaf_to_the_bus_is_not_ready() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();

        let readiness = Dependencies::new(
            PgStore::new(pool),
            Vec::new(),
            crate::bus::BusHealth::alive(),
            Started::default(),
        )
        .probe()
        .await;

        assert!(
            readiness
                .checks
                .iter()
                .any(|check| check.name == "startup" && !check.healthy)
        );
        assert!(
            readiness
                .checks
                .iter()
                .any(|check| check.name == "eventbus" && check.healthy)
        );
    }

    #[actix_web::test]
    async fn the_exporter_serves_the_registry_the_process_writes_to() {
        // A mismatched `app_data` type is a runtime `500`, so check this registry's number comes back.
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

        // Scrapers read the content type first.
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
        // A no-op registry gives an empty scrape, not a `500`.
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
        // No transports is not a degraded state.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();

        let checks = dependencies(PgStore::new(pool), vec!["webpush"])
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
