//! Traces, and where they go (design §13).
//!
//! Logs answer "what happened here"; a trace answers "what happened to *this
//! request*, everywhere". The difference is the whole reason this module exists:
//! `publish → fanout → push transport` crosses three layers and a background loop,
//! and without spans an incident is reconstructed by correlating log lines by hand —
//! bearable on one replica, not on the third.
//!
//! # Off unless somebody asked
//!
//! No `[telemetry]` section means no exporter, no span processor and no background
//! thread: the process emits structured logs exactly as before. That is the default
//! because a collector endpoint is a fact about a deployment — a built-in
//! `localhost:4318` would make every instance without a collector retry an export
//! nobody asked for, forever.
//!
//! # Why the layer is installed here and not next to the spans
//!
//! The spans themselves are plain `tracing` — `#[instrument]` on the use cases, and
//! `notify-app` has no idea whether anything is listening. Same rule as the metrics
//! port: what is emitted belongs to the code, where it goes belongs to the
//! composition root.

use anyhow::Context as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::config::Telemetry;

/// The OTLP/HTTP path every collector serves traces on.
///
/// Appended here rather than left to the operator, and worth a constant because the
/// library only appends it to endpoints it reads from the environment — an endpoint
/// passed programmatically is used verbatim. Configuring `http://collector:4318`
/// and getting silent 404s is the footgun this removes.
const TRACES_PATH: &str = "/v1/traces";

/// Whatever has to stay alive for spans to keep being exported, and be flushed
/// before the process leaves.
///
/// [`Traces::shutdown`] is not optional politeness: the batch processor holds
/// finished spans until its next tick, so a process that exits without it drops
/// precisely the spans of the request that made somebody restart it.
pub(crate) struct Traces {
    provider: Option<SdkTracerProvider>,
}

impl Traces {
    /// Flush and stop the exporter. A no-op when nothing was configured.
    pub(crate) fn shutdown(self) {
        if let Some(provider) = self.provider
            && let Err(error) = provider.shutdown()
        {
            // Reported and not propagated: the process is already on its way out,
            // and failing a clean shutdown over unexported telemetry would turn a
            // stop into a crash.
            tracing::warn!(%error, "the trace exporter did not shut down cleanly");
        }
    }
}

/// Install the subscriber, with an OTLP layer when `[telemetry]` asks for one.
///
/// Called once, before anything else logs — including configuration loading, which
/// is why it takes the section rather than the whole [`crate::config::Config`]: the
/// caller has to have read the file already, and everything between the two is
/// silent.
pub(crate) fn install(telemetry: Option<&Telemetry>) -> anyhow::Result<Traces> {
    // Unchanged from before there were traces: JSON lines, and a filter an operator
    // can widen with `RUST_LOG` without a rebuild.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=warn"));
    let logs = tracing_subscriber::fmt::layer().json();

    let Some(telemetry) = telemetry else {
        tracing_subscriber::registry()
            .with(filter)
            .with(logs)
            .init();
        return Ok(Traces { provider: None });
    };

    let provider = provider(telemetry)?;
    let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("notify-server"));

    tracing_subscriber::registry()
        .with(filter)
        .with(logs)
        .with(layer)
        .init();

    // W3C `traceparent`, in both directions: it is what the HTTP middleware reads
    // off an incoming request (§13 — a CI job can link its own trace to the
    // notification it publishes) and what an outgoing call would carry. Set on the
    // global rather than passed around, because that is the only thing the
    // propagation API offers.
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    tracing::info!(
        endpoint = %telemetry.otlp_endpoint,
        service = %telemetry.service_name,
        sample_ratio = telemetry.sample_ratio,
        "exporting traces over OTLP/HTTP"
    );

    Ok(Traces {
        provider: Some(provider),
    })
}

/// Read a W3C `traceparent` off an actix request.
///
/// `opentelemetry-http` ships one of these for `http::HeaderMap`, and actix has its
/// own header map, so the ten lines are cheaper than the dependency.
struct Headers<'a>(&'a actix_web::http::header::HeaderMap);

impl opentelemetry::propagation::Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0
            .keys()
            .map(actix_web::http::header::HeaderName::as_str)
            .collect()
    }
}

/// Continue the caller's trace, or start one (design §13).
///
/// # Why an incoming `traceparent` is trusted
///
/// It is a correlation id, not a credential: the worst a forged one does is attach
/// this request to a trace it does not belong to, in a system the sender can already
/// write to by publishing. Refusing it would cost the thing the header is for — a CI
/// job linking its own pipeline trace to the notification it published (§13) — and
/// buy nothing, because the sampling decision it carries is the caller's to make
/// about their own trace.
///
/// The span is created whether or not an exporter exists. With `[telemetry]` absent
/// the propagator is a no-op, the parent context is empty, and what remains is an
/// ordinary `tracing` span that costs a field or two on the log lines beneath it.
pub(crate) async fn continue_trace(
    request: actix_web::dev::ServiceRequest,
    next: actix_web::middleware::Next<impl actix_web::body::MessageBody>,
) -> Result<actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>, actix_web::Error> {
    use tracing::Instrument as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let parent = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&Headers(request.headers()))
    });

    // The route pattern rather than the path: `/api/v1/notifications/{id}` is one
    // operation, and the id-per-span spelling would make the collector's list of
    // operations as long as the table.
    let span = tracing::info_span!(
        "http",
        method = %request.method(),
        route = request.match_pattern().unwrap_or_else(|| "unmatched".to_owned()),
    );
    // The error case is "no subscriber is collecting spans", which is every build
    // without `[telemetry]` and is not something to report once per request.
    let _ = span.set_parent(parent);

    next.call(request).instrument(span).await
}

/// Build the exporter and the provider that feeds it.
fn provider(telemetry: &Telemetry) -> anyhow::Result<SdkTracerProvider> {
    anyhow::ensure!(
        !telemetry.otlp_endpoint.trim().is_empty(),
        "[telemetry] is present but otlp_endpoint is empty — remove the section or \
         point it at a collector"
    );
    anyhow::ensure!(
        (0.0..=1.0).contains(&telemetry.sample_ratio),
        "[telemetry] sample_ratio must be between 0.0 and 1.0, got {}",
        telemetry.sample_ratio
    );

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint(&telemetry.otlp_endpoint))
        .build()
        .context("building the OTLP span exporter")?;

    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        // Parent-based: a child follows the decision taken for its root, here or in
        // the caller that sent us a `traceparent`. Sampling each span on its own
        // would produce traces with holes in the middle, which are worse to read
        // than no trace at all.
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            telemetry.sample_ratio,
        ))))
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name(telemetry.service_name.clone())
                .build(),
        )
        .build())
}

/// The collector's base URL, with the traces path it documents.
fn endpoint(configured: &str) -> String {
    let base = configured.trim_end_matches('/');
    if base.ends_with(TRACES_PATH) {
        base.to_owned()
    } else {
        format!("{base}{TRACES_PATH}")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    /// What the middleware saw, smuggled out of the request it ran in.
    static SEEN: std::sync::Mutex<Option<(String, Option<String>)>> = std::sync::Mutex::new(None);

    #[actix_web::test]
    async fn the_middleware_knows_which_route_it_is_on() {
        // The reason this is a test and not a comment: an app-level middleware runs
        // around routing, and whether `match_pattern()` is populated by the time it
        // asks decides whether the span carries `/api/v1/notifications/{id}` or the
        // useless `unmatched` for every request in the deployment. Guessing was not
        // an option; this pins the answer, so an actix upgrade that changes it fails
        // here rather than degrading a collector nobody is reading yet.
        async fn probe(
            request: actix_web::dev::ServiceRequest,
            next: actix_web::middleware::Next<impl actix_web::body::MessageBody>,
        ) -> Result<
            actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>,
            actix_web::Error,
        > {
            *SEEN.lock().unwrap() = Some((request.path().to_owned(), request.match_pattern()));
            next.call(request).await
        }

        let app = actix_web::test::init_service(
            actix_web::App::new()
                .wrap(actix_web::middleware::from_fn(probe))
                .route(
                    "/things/{id}",
                    actix_web::web::get().to(actix_web::HttpResponse::Ok),
                ),
        )
        .await;

        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get()
                .uri("/things/42")
                .to_request(),
        )
        .await;
        assert!(response.status().is_success());

        let (path, pattern) = SEEN.lock().unwrap().clone().unwrap();
        assert_eq!(path, "/things/42");
        assert_eq!(
            pattern.as_deref(),
            Some("/things/{id}"),
            "the span's `route` is the pattern; if this ever goes None, the \
             attribute has to come from somewhere else or be dropped"
        );
    }

    #[test]
    fn a_collector_base_url_gains_the_path_and_never_gains_it_twice() {
        // Both spellings appear in collector documentation, and the difference
        // between them is silent 404s rather than an error anybody sees.
        assert_eq!(
            endpoint("http://collector:4318"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            endpoint("http://collector:4318/"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            endpoint("http://collector:4318/v1/traces"),
            "http://collector:4318/v1/traces"
        );
    }

    #[test]
    fn a_section_that_names_no_collector_is_refused_rather_than_exporting_nowhere() {
        let empty = Telemetry {
            otlp_endpoint: String::new(),
            ..Telemetry::default()
        };
        assert!(
            provider(&empty).is_err(),
            "an empty endpoint is a typo, not a mode"
        );
    }

    #[test]
    fn a_ratio_outside_the_unit_interval_stops_the_process() {
        // 0 and 1 are both meaningful — "record nothing" and "record everything" —
        // so the check is on the interval, not on zero.
        let over = Telemetry {
            otlp_endpoint: "http://collector:4318".to_owned(),
            sample_ratio: 1.5,
            ..Telemetry::default()
        };
        assert!(provider(&over).is_err());

        let under = Telemetry {
            otlp_endpoint: "http://collector:4318".to_owned(),
            sample_ratio: -0.1,
            ..Telemetry::default()
        };
        assert!(provider(&under).is_err());
    }
}
