//! `GET /cli`: download the `notify` CLI binary built alongside this server (design §9).

use actix_web::{HttpResponse, get, web};

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// Download the `notify` CLI binary this server was built with; any authenticated caller.
#[utoipa::path(
    get,
    path = "/cli",
    tag = "cli",
    responses(
        (status = 200, description = "The `notify` binary (Linux)", content_type = "application/octet-stream"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 501, description = "This deployment does not serve it (no [cli] section)", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/cli")]
pub(crate) async fn download(
    state: web::Data<ApiState>,
    _caller: Authenticated,
) -> Result<HttpResponse, ApiError> {
    let binary = state
        .cli_binary
        .clone()
        .ok_or(notify_ports::PortError::NotConfigured { what: "cli" })?;

    let mut response = HttpResponse::Ok();
    response
        .content_type("application/octet-stream")
        .insert_header(("content-disposition", "attachment; filename=\"notify\""))
        // Executables must never be sniffed into something a browser will render.
        .insert_header(("x-content-type-options", "nosniff"));

    // Digest + strong ETag, to check against the digest published with the release.
    if let Some(digest) = &state.cli_digest {
        response
            .insert_header(("x-content-digest", format!("sha-256={digest}")))
            .insert_header(("etag", format!("\"{digest}\"")));
    }

    Ok(response.body(binary))
}
