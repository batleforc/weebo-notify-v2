//! The OpenAPI document (design §8).
//!
//! Utoipa builds this from the annotations sitting on the handlers, so the spec is
//! written as the API is written rather than maintained alongside it. The document
//! is served on `/api/docs` (Scalar UI) and exported by `notify-server dump-openapi`.

use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    paths(crate::health::healthz, crate::health::readyz),
    info(
        title = "weebo-notify",
        description = "Rich, multi-channel, multi-device notification platform. \
                       Notifications are lifecycle objects: they carry state, accumulate \
                       revisions, expose actions and collect receipts.",
        license(name = "MPL-2.0", url = "https://www.mozilla.org/en-US/MPL/2.0/")
    ),
    servers((url = "/api/v1", description = "Versioned API root")),
    tags(
        (name = "channels", description = "Channels, their members and their policy"),
        (name = "notifications", description = "Publishing, revisions, interactions"),
        (name = "sync", description = "Offline backlog and real-time streams"),
        (name = "devices", description = "Device enrolment and revocation"),
        (name = "auth", description = "API keys, service accounts, CI federation"),
        (name = "admin", description = "Platform console — metadata and operations only"),
    )
)]
struct ApiDoc;

/// Build the OpenAPI document. Handler modules register their paths here as the
/// API surface grows through M1.
pub(crate) fn document() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}
