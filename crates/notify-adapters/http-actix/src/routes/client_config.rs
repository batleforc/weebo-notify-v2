//! `GET /client-config`: what a native client needs before it can sign in to this
//! instance (RFC 0001). Public by design: it is read before there is a token, and
//! every value in it is already public in the web image's `env.js`.

use actix_web::{HttpResponse, get, web};
use serde::Serialize;
use utoipa::ToSchema;

use crate::state::ApiState;

/// The public client settings of this instance.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ClientConfig {
    /// The OIDC issuer this server validates tokens against (`[oidc] issuer`).
    #[schema(example = "https://auth.example.org/application/o/weebo-notify/")]
    pub oidc_issuer: String,
    /// The public client to sign in as (`[oidc] client_id`).
    #[schema(example = "weebo-notify")]
    pub oidc_client_id: String,
    /// Where attachment downloads are served from, when that is not the API itself.
    #[schema(example = "https://files.example.org")]
    pub attachment_origin: Option<String>,
}

/// The settings a native client needs to sign in to this instance; no credential.
#[utoipa::path(
    get,
    path = "/client-config",
    tag = "clients",
    responses(
        (status = 200, description = "Issuer, client id and attachment origin of this instance", body = ClientConfig),
    ),
    security(())
)]
#[get("/client-config")]
pub(crate) async fn client_config(state: web::Data<ApiState>) -> HttpResponse {
    HttpResponse::Ok()
        // Changes only with the server's configuration; a client re-reads it on next probe.
        .insert_header(("cache-control", "public, max-age=300"))
        .json(&state.client_config)
}
