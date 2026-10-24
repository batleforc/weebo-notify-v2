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
    // Whether the middleware *reads* it is a separate decision — see
    // `continue_trace`.
    #[expect(
        clippy::let_underscore_must_use,
        reason = "set once at startup; a second `init` (tests) keeps the first value"
    )]
    let _ = TRUST_INBOUND.set(telemetry.trust_inbound_trace_context);

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

/// `[telemetry] trust_inbound_trace_context`, read once by [`install`]; a global because the
/// middleware is a plain function and the value never changes after boot. Unset reads as `false`.
static TRUST_INBOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

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

/// Continue the caller's trace, or start one (design §13). An incoming `traceparent` is continued
/// only with `trust_inbound_trace_context`: its `sampled` flag would otherwise let any caller
/// force 100% sampling; the ratio still decides either way. Without an exporter this is a plain
/// `tracing` span.
pub(crate) async fn continue_trace(
    request: actix_web::dev::ServiceRequest,
    next: actix_web::middleware::Next<impl actix_web::body::MessageBody>,
) -> Result<actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>, actix_web::Error> {
    use tracing::Instrument as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let parent = if TRUST_INBOUND.get().copied().unwrap_or(false) {
        opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.extract(&Headers(request.headers()))
        })
    } else {
        opentelemetry::Context::new()
    };

    // The route pattern, not the path, keeps the operation list bounded.
    let span = tracing::info_span!(
        "http",
        method = %request.method(),
        route = request.match_pattern().unwrap_or_else(|| "unmatched".to_owned()),
    );
    #[expect(
        clippy::let_underscore_must_use,
        reason = "errors only when no subscriber collects spans: not worth reporting"
    )]
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
        .with_sampler(sampler(telemetry.sample_ratio))
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name(telemetry.service_name.clone())
                .build(),
        )
        .build())
}

/// Parent-based for parents created *here*, ratio-based for everything else.
///
/// A local child follows its root: sampling each span on its own would produce
/// traces with holes in the middle, which are worse to read than no trace at all.
/// A *remote* parent — the `traceparent` a caller sent — does not get to decide:
/// its `sampled` flag is a header anybody can set, so the ratio is applied to it as
/// to a fresh root. The trace id is still the caller's, so their trace and ours
/// join whenever both sides kept it.
fn sampler(ratio: f64) -> LocalParentOrRatio {
    LocalParentOrRatio(ratio)
}

/// See [`sampler`]. The SDK's `ParentBased` treats a remote parent like a local
/// one and offers no switch, hence these few lines.
#[derive(Debug, Clone)]
struct LocalParentOrRatio(f64);

impl opentelemetry_sdk::trace::ShouldSample for LocalParentOrRatio {
    fn should_sample(
        &self,
        parent_context: Option<&opentelemetry::Context>,
        trace_id: opentelemetry::trace::TraceId,
        name: &str,
        span_kind: &opentelemetry::trace::SpanKind,
        attributes: &[opentelemetry::KeyValue],
        links: &[opentelemetry::trace::Link],
    ) -> opentelemetry_sdk::trace::SamplingResult {
        use opentelemetry::trace::TraceContextExt as _;

        use opentelemetry_sdk::trace::IdGenerator as _;

        let local_parent = parent_context
            .is_some_and(|cx| cx.has_active_span() && !cx.span().span_context().is_remote());
        if local_parent {
            // `ParentBased` over a delegate it never consults when there is a
            // parent: that is exactly "follow the parent", trace state included.
            return Sampler::ParentBased(Box::new(Sampler::AlwaysOff)).should_sample(
                parent_context,
                trace_id,
                name,
                span_kind,
                attributes,
                links,
            );
        }
        // The ratio sampler compares the trace id's low bits against the ratio,
        // and a remote parent's trace id is the caller's to choose: one ending in
        // zeros is sampled at any ratio above zero. So the draw is made on a fresh
        // random id, never on the one that came in a header — the span still
        // carries the caller's trace id, only the decision ignores it.
        let draw = opentelemetry_sdk::trace::RandomIdGenerator::default().new_trace_id();
        Sampler::TraceIdRatioBased(self.0).should_sample(
            parent_context,
            draw,
            name,
            span_kind,
            attributes,
            links,
        )
    }
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
    fn a_remote_parent_marked_sampled_does_not_force_a_trace() {
        use opentelemetry::trace::{
            SpanContext, SpanId, SpanKind, TraceContextExt as _, TraceFlags, TraceId, TraceState,
        };
        use opentelemetry_sdk::trace::{SamplingDecision, ShouldSample as _};

        let trace_id = TraceId::from_bytes([7; 16]);
        let remote = SpanContext::new(
            trace_id,
            SpanId::from_bytes([1; 8]),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        let parent = opentelemetry::Context::new().with_remote_span_context(remote);

        let decision = sampler(0.0)
            .should_sample(Some(&parent), trace_id, "http", &SpanKind::Server, &[], &[])
            .decision;
        assert_eq!(
            decision,
            SamplingDecision::Drop,
            "a header anybody can send must not override sample_ratio = 0"
        );

        // A parent created here is still followed, or traces would have holes.
        let local = SpanContext::new(
            trace_id,
            SpanId::from_bytes([2; 8]),
            TraceFlags::SAMPLED,
            false,
            TraceState::default(),
        );
        let parent = opentelemetry::Context::new().with_remote_span_context(local);
        let decision = sampler(0.0)
            .should_sample(
                Some(&parent),
                trace_id,
                "child",
                &SpanKind::Internal,
                &[],
                &[],
            )
            .decision;
        assert_eq!(decision, SamplingDecision::RecordAndSample);
    }

    #[test]
    fn a_caller_cannot_pick_a_trace_id_that_forces_sampling() {
        use opentelemetry::trace::{
            SpanContext, SpanId, SpanKind, TraceContextExt as _, TraceFlags, TraceId, TraceState,
        };
        use opentelemetry_sdk::trace::{SamplingDecision, ShouldSample as _};

        // Low bits all zero: the id a ratio sampler reading the trace id would
        // sample at every ratio above zero.
        let chosen = TraceId::from_bytes([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let remote = SpanContext::new(
            chosen,
            SpanId::from_bytes([1; 8]),
            TraceFlags::default(),
            true,
            TraceState::default(),
        );
        let parent = opentelemetry::Context::new().with_remote_span_context(remote);

        let sampled = (0..1_000)
            .filter(|_| {
                sampler(0.01)
                    .should_sample(Some(&parent), chosen, "http", &SpanKind::Server, &[], &[])
                    .decision
                    == SamplingDecision::RecordAndSample
            })
            .count();
        // About 10 expected; 1 000 would be the forced outcome. The bound is loose
        // enough that a fair coin never trips it.
        assert!(sampled < 60, "sampled {sampled} of 1000 at ratio 0.01");
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
