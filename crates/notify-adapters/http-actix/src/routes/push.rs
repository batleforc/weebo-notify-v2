//! What a browser needs before it can subscribe (design §6.2, §11 `[push.webpush]`).
//!
//! RFC 8292 makes the server's public key an argument of `pushManager.subscribe()`:
//! without it a PWA cannot obtain a subscription at all, so the endpoint that hands
//! it out is a precondition of the whole Web Push path rather than a convenience.
//!
//! # Why it is served and not built into the client
//!
//! §8 did not list this route — the design describes the transport and the
//! configuration, and left the gap between them unnamed. The alternative was a build
//! argument in `web/`, and it is worse in the way that matters here: a deployment
//! rotating its VAPID key would need a rebuilt frontend to keep push working, and
//! ADR-0009's rule that a client is derived from the contract would be broken by the
//! one value that decides whether pushes arrive. Serving it keeps the key where the
//! rest of the deployment's configuration already is (§11).
//!
//! A public value behind authentication: nothing is protected by that — every
//! subscriber holds a copy — but nothing here is public either (§5.1), and the only
//! caller subscribes after login anyway.
//!
//! `501` when the deployment runs no Web Push, which is [`PortError::NotConfigured`]
//! spelled by the same mapping as every other missing capability (§14.3): a PWA is
//! told "this server does not do that" instead of being handed a key that does not
//! exist, and it can then skip the subscription rather than retry it.

use actix_web::{get, web};
use notify_ports::PortError;
use serde::Serialize;
use utoipa::ToSchema;

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// The identity of this server, as a push service knows it.
#[derive(Debug, Serialize, ToSchema)]
pub struct WebPushKey {
    /// The uncompressed P-256 public point, base64url — passed verbatim as
    /// `applicationServerKey` (RFC 8292).
    #[schema(example = "BEl62iUYgUivxIkv69yViEuiBIa-Ib9-SkFbx4gpXYm…")]
    pub application_server_key: String,
}

/// The key this server subscribes browsers with.
#[utoipa::path(
    get,
    path = "/push/webpush",
    tag = "devices",
    responses(
        (status = 200, description = "The `applicationServerKey` for `pushManager.subscribe()`", body = WebPushKey),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 501, description = "This deployment runs no Web Push transport", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/push/webpush")]
pub(crate) async fn webpush_key(
    state: web::Data<ApiState>,
    _caller: Authenticated,
) -> Result<web::Json<WebPushKey>, ApiError> {
    let key = state
        .application_server_key
        .clone()
        .ok_or(PortError::NotConfigured {
            what: "push.webpush",
        })?;

    Ok(web::Json(WebPushKey {
        application_server_key: key,
    }))
}
