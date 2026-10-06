//! The OpenAPI document (design §8), served on `/api/docs` and exported by `dump-openapi`.
//! Paths come from the actix route table itself, built statelessly, so the export needs no infra.

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
        license(
            name = "Apache-2.0",
            url = "https://www.apache.org/licenses/LICENSE-2.0"
        )
    ),
    tags(
        (name = "channels", description = "Channels, their members and their policy"),
        (name = "notifications", description = "Publishing, revisions, interactions"),
        (name = "sync", description = "Offline backlog and real-time streams"),
        (name = "devices", description = "Device enrolment and revocation"),
        (name = "auth", description = "API keys, service accounts, CI federation"),
        (name = "admin", description = "Platform console — metadata and operations only"),
        (name = "cli", description = "Downloading the `notify` CLI binary"),
        (name = "compat", description = "Gotify, ntfy and Alertmanager compatible publishing"),
    )
)]
struct ApiDoc;

/// One `Bearer` scheme for both OIDC tokens and API keys (design §5.2).
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

/// The document before the route table is folded in; shared by the server and [`document`].
pub(crate) fn base() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// Build the OpenAPI document.
pub(crate) fn document() -> utoipa::openapi::OpenApi {
    // The `App` is only for its collected paths; no handler runs.
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

        // Without these the generated clients get untyped bodies (ADR-0013).
        for schema in ["Notification", "Channel", "Problem"] {
            assert!(
                components.schemas.contains_key(schema),
                "{schema} is not in the document"
            );
        }
    }
}
