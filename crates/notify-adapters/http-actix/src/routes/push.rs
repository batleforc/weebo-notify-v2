//! The VAPID public key a browser needs to subscribe (design §6.2, RFC 8292), served
//! rather than built into the client so key rotation needs no rebuild; `501` without Web Push.

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
    /// The uncompressed P-256 public point, base64url: `applicationServerKey` (RFC 8292).
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
