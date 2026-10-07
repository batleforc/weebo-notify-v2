//! A person's live realtime connections, across every replica, and the way to cut one.

use actix_web::{HttpRequest, HttpResponse, delete, get, web};
use serde::Serialize;
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::routes::stream::fingerprint;
use crate::state::ApiState;

/// How long a disconnected credential stays off realtime, at most.
const BLOCK_FOR: time::Duration = time::Duration::hours(12);

/// One live connection.
#[derive(Debug, Serialize, ToSchema)]
pub struct Connection {
    /// The id `DELETE /streams/{id}` takes.
    pub id: String,
    /// `sse` or `websocket`.
    pub transport: String,
    /// What the client calls itself (its `User-Agent`).
    pub client: String,
    /// How it signed in: `session`, `key`, `device:<id>` or `ci`.
    pub credential: String,
    /// When it connected.
    #[serde(with = "time::serde::rfc3339")]
    pub opened_at: OffsetDateTime,
    /// Whether it uses the credential of this very request (this browser, this CLI).
    pub current: bool,
}

/// Every realtime connection the caller holds, oldest first.
#[utoipa::path(
    get,
    path = "/streams",
    tag = "sync",
    responses(
        (status = 200, description = "The connections", body = Vec<Connection>),
        (status = 401, description = "No usable credential", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/streams")]
pub(crate) async fn list_streams(
    state: web::Data<ApiState>,
    caller: Authenticated,
    request: HttpRequest,
) -> Result<web::Json<Vec<Connection>>, ApiError> {
    let mine = fingerprint(
        request
            .headers()
            .get(actix_web::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
    );
    let leases = state
        .streams
        .list(&caller.0.principal)
        .await
        .map_err(notify_ports::Failure::from)?;
    Ok(web::Json(
        leases
            .into_iter()
            .map(|lease| Connection {
                current: lease.fingerprint == mine,
                id: lease.id,
                transport: lease.transport,
                client: lease.client,
                credential: lease.credential,
                opened_at: lease.opened_at,
            })
            .collect(),
    ))
}

/// Disconnect one of the caller's connections. Its credential is kept off realtime
/// for up to 12 h; to remove access for good, revoke the key or the device.
#[utoipa::path(
    delete,
    path = "/streams/{id}",
    tag = "sync",
    params(("id" = String, Path, description = "The connection id")),
    responses(
        (status = 204, description = "Disconnected, or already gone"),
        (status = 401, description = "No usable credential", body = Problem),
    ),
    security(("bearer" = []))
)]
#[delete("/streams/{id}")]
pub(crate) async fn disconnect_stream(
    state: web::Data<ApiState>,
    caller: Authenticated,
    id: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    state
        .streams
        .disconnect(
            &caller.0.principal,
            &id,
            OffsetDateTime::now_utc() + BLOCK_FOR,
        )
        .await
        .map_err(notify_ports::Failure::from)?;
    Ok(HttpResponse::NoContent().finish())
}
