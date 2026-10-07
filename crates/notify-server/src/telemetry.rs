//! Traces and where they go (design §13). Off unless `[telemetry]` is set; spans are plain `tracing`
//! in the code, the exporter is wired only here.

use anyhow::Context as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::config::Telemetry;

/// The OTLP/HTTP traces path, appended since the library uses programmatic endpoints verbatim.
const TRACES_PATH: &str = "/v1/traces";

/// What must stay alive for export; call [`Traces::shutdown`] before exit or the last spans are lost.
pub(crate) struct Traces {
    provider: Option<SdkTracerProvider>,
}

impl Traces {
    /// Flush and stop the exporter. A no-op when nothing was configured.
    pub(crate) fn shutdown(self) {
        if let Some(provider) = self.provider
            && let Err(error) = provider.shutdown()
        {
            // Reported, not propagated: a failed flush must not turn a stop into a crash.
            tracing::warn!(%error, "the trace exporter did not shut down cleanly");
        }
    }
}

/// Install the subscriber, with an OTLP layer when `[telemetry]` asks for one; call once, first.
pub(crate) fn install(telemetry: Option<&Telemetry>) -> anyhow::Result<Traces> {
    // JSON lines, filterable with `RUST_LOG`.
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

    // W3C `traceparent` on the global propagator, so CI jobs can link their traces (§13).
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

/// Read a W3C `traceparent` off an actix request (cheaper than the `opentelemetry-http` dependency).
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

/// Continue the caller's trace, or start one (design §13). An incoming `traceparent` is trusted:
/// it is a correlation id, not a credential. Without an exporter this is a plain `tracing` span.
pub(crate) async fn continue_trace(
    request: actix_web::dev::ServiceRequest,
    next: actix_web::middleware::Next<impl actix_web::body::MessageBody>,
) -> Result<actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>, actix_web::Error> {
    use tracing::Instrument as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let parent = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&Headers(request.headers()))
    });

    // The route pattern, not the path, keeps the operation list bounded.
    let span = tracing::info_span!(
        "http",
        method = %request.method(),
        route = request.match_pattern().unwrap_or_else(|| "unmatched".to_owned()),
    );
    // Errors only when no subscriber collects spans: not worth reporting.
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
        // Parent-based, so traces have no holes.
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
        // Pins that `match_pattern()` is populated in app-level middleware, across actix upgrades.
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
        // Both spellings occur; the wrong one is a silent 404.
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
        // 0 and 1 are both meaningful, so the check is on the interval.
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
