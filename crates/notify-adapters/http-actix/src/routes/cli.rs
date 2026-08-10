//! `GET /cli` — download the `notify` CLI binary (design §9, new).
//!
//! Linux only for now: the full artefact matrix (macOS/Windows, cross-arch) is
//! milestone M5's job. This serves whatever `[cli] binary_path` points the
//! running server at, which is always the binary built alongside this same
//! server in the same Containerfile stage — so the two can never disagree on
//! version, and there is nothing to reconcile.

use actix_web::{HttpResponse, get, web};

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// Download the `notify` CLI binary this server was built with.
///
/// No scope beyond "logged in" (`Authenticated` alone, the same weakest
/// requirement `GET /whoami` uses) — a person and a CI service account are
/// equally allowed to fetch it, which is why the principal is never narrowed.
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

    // What was served, so it can be checked against the digest published with the
    // release rather than trusted because the connection looked fine. A strong
    // ETag as well, because that is the header caches already understand.
    if let Some(digest) = &state.cli_digest {
        response
            .insert_header(("x-content-digest", format!("sha-256={digest}")))
            .insert_header(("etag", format!("\"{digest}\"")));
    }

    Ok(response.body(binary))
}
