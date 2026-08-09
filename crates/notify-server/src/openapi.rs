//! The OpenAPI document (design §8).
//!
//! Utoipa builds this from the annotations sitting on the handlers, so the spec is
//! written as the API is written rather than maintained alongside it. The document
//! is served on `/api/docs` (Scalar UI) and exported by `notify-server dump-openapi`.
//!
//! The paths come from the route table itself rather than from a list kept here: an
//! actix `App` is assembled purely to be taken apart again, which is what makes
//! "served but undocumented" unrepresentable. Building that `App` needs no state, so
//! the export runs with no database, no object store and no identity provider — the
//! property the `contract` CI job depends on (design §3.2).

use actix_web::App;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_actix_web::AppExt as _;

#[derive(OpenApi)]
#[openapi(
    paths(crate::health::healthz, crate::health::readyz),
    modifiers(&BearerAuth),
    info(
        title = "weebo-notify",
        description = "Rich, multi-channel, multi-device notification platform. \
                       Notifications are lifecycle objects: they carry state, accumulate \
                       revisions, expose actions and collect receipts.",
        license(name = "MPL-2.0", url = "https://www.mozilla.org/en-US/MPL/2.0/")
    ),
    tags(
        (name = "channels", description = "Channels, their members and their policy"),
        (name = "notifications", description = "Publishing, revisions, interactions"),
        (name = "sync", description = "Offline backlog and real-time streams"),
        (name = "devices", description = "Device enrolment and revocation"),
        (name = "auth", description = "API keys, service accounts, CI federation"),
        (name = "admin", description = "Platform console — metadata and operations only"),
        (name = "cli", description = "Downloading the `notify` CLI binary"),
    )
)]
struct ApiDoc;

/// One scheme for both credentials.
///
/// An OIDC access token and an API key both arrive as `Authorization: Bearer`
/// (design §5.2). Declaring two schemes would suggest two headers, and the
/// generated clients would grow two ways to say the same thing.
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_default();
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "An OIDC access token, or an API key of the form \
                         `wnk_<prefix>_<secret>`.",
                    ))
                    .build(),
            ),
        );
    }
}

/// The document before the route table is folded into it.
///
/// The running server assembles its own `App` and needs to seed it with this;
/// [`document`] builds a throwaway one for the export. Two callers, one definition.
pub(crate) fn base() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// Build the OpenAPI document.
pub(crate) fn document() -> utoipa::openapi::OpenApi {
    // The `App` is discarded: only the paths it collected are wanted. No handler
    // runs, so the absent application state is never reached.
    let (_app, api) = App::new()
        .into_utoipa_app()
        .openapi(ApiDoc::openapi())
        .configure(notify_http_actix::configure)
        .split_for_parts();

    api
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    #[test]
    fn the_document_carries_the_whole_api_and_not_just_the_probes() {
        let document = document();

        assert!(
            document.paths.paths.len() > 15,
            "the export lost the route table: {} paths",
            document.paths.paths.len()
        );
        assert!(document.paths.paths.contains_key("/healthz"));
        assert!(document.paths.paths.contains_key("/api/v1/notifications"));
    }

    #[test]
    fn both_credentials_are_described_as_the_one_header_they_are() {
        let document = document();
        let components = document.components.unwrap();

        assert!(
            components.security_schemes.contains_key("bearer"),
            "without a scheme the generated clients have no way to send a credential"
        );
        assert_eq!(components.security_schemes.len(), 1);
    }

    #[test]
    fn the_domain_schemas_travelled_with_it() {
        let document = document();
        let components = document.components.unwrap();

        // If these are missing the generated clients fall back to untyped bodies,
        // and the whole point of ADR-0013 is lost.
        for schema in ["Notification", "Channel", "Problem"] {
            assert!(
                components.schemas.contains_key(schema),
                "{schema} is not in the document"
            );
        }
    }
}
