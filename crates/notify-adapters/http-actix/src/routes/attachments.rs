//! `POST /attachments/presign` and `GET /attachments/{id}` (design §6.1, §8): signed
//! credentials only; bytes go client to bucket and back, never through here.

use actix_web::{HttpResponse, get, post, web};
use notify_domain::{AttachmentId, ChannelId};
use notify_ports::{PresignedDownload, PresignedUpload};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// What a client asks to upload.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct PresignBody {
    /// Where the publisher intends to publish it; requires `publish` there (design §6.1).
    pub(crate) channel: String,
    /// The declared media type, imposed in the signature.
    pub(crate) content_type: String,
    /// The declared size, checked against §8.2's ceiling **before** signing.
    pub(crate) content_length: u64,
}

/// Sign an upload to the object store.
#[utoipa::path(
    post,
    path = "/attachments/presign",
    tag = "notifications",
    request_body = PresignBody,
    responses(
        (status = 200, description = "A short-lived signed upload", body = PresignedUpload),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Cannot publish in that channel", body = Problem),
        (status = 413, description = "Over the 25 MiB ceiling (§8.2)", body = Problem),
        (status = 422, description = "Not a supported image type", body = Problem),
        (status = 501, description = "This deployment has no object store", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/attachments/presign")]
pub(crate) async fn presign(
    state: web::Data<ApiState>,
    caller: Authenticated,
    body: web::Json<PresignBody>,
) -> Result<web::Json<PresignedUpload>, ApiError> {
    let body = body.into_inner();
    let channel: ChannelId = body.channel.parse()?;

    Ok(web::Json(
        state
            .presign
            .execute(&caller.0, &channel, &body.content_type, body.content_length)
            .await?,
    ))
}

/// Sign a download of an attachment a notification shows.
#[utoipa::path(
    get,
    operation_id = "presign_download",
    path = "/attachments/{id}",
    tag = "notifications",
    params(("id" = String, Path, description = "The attachment id a payload references")),
    responses(
        (status = 200, description = "A short-lived signed download", body = PresignedDownload),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 409, description = "No such attachment, or none the caller may see — \
                                      deliberately the same answer (`notification.gone`)", body = Problem),
        (status = 501, description = "This deployment has no object store", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/attachments/{id}")]
pub(crate) async fn download(
    state: web::Data<ApiState>,
    caller: Authenticated,
    id: web::Path<String>,
) -> Result<web::Json<PresignedDownload>, ApiError> {
    let id: AttachmentId = id.into_inner().parse()?;

    Ok(web::Json(state.attachment.execute(&caller.0, id).await?))
}

/// Fetch the small square derived from an attachment (design §2.3, §10.3, D30); bytes
/// rather than a signed URL, to avoid a signing round trip per rendered row.
#[utoipa::path(
    get,
    path = "/attachments/{id}/thumbnail",
    tag = "notifications",
    params(("id" = String, Path, description = "The attachment id a payload references")),
    responses(
        (status = 200, description = "The thumbnail", content_type = "image/png"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 409, description = "No such attachment, none the caller may see, or \
                                      nothing was ever derived from it — deliberately \
                                      the same answer (`notification.gone`)", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/attachments/{id}/thumbnail")]
pub(crate) async fn thumbnail(
    state: web::Data<ApiState>,
    caller: Authenticated,
    id: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let id: AttachmentId = id.into_inner().parse()?;
    let object = state.attachment_thumbnail.execute(&caller.0, id).await?;

    Ok(crate::routes::served_bytes(&mut HttpResponse::Ok())
        .content_type(object.content_type)
        // Immutable: an attachment is never replaced in place (§2.3).
        .insert_header(("cache-control", "private, max-age=86400, immutable"))
        .body(object.bytes))
}
