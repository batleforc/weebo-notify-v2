//! `POST /attachments/presign` and `GET /attachments/{id}` — the way in and out of
//! the bucket (design §6.1, §8).
//!
//! Two routes, one per direction, and neither carries bytes: they go **client →
//! bucket** and **bucket → client**, never through here (§6.1), so there is no
//! upload handler to write and no 25 MiB body to hold. What each returns is a
//! credential with an expiry, which is why every check lives in the use case
//! behind it rather than anywhere downstream — after the signature the client is
//! talking to the bucket, not to us.

use actix_web::{get, post, web};
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
    /// Where the publisher intends to publish it.
    ///
    /// Required, and it is the authorisation: uploading is a publisher's act, not
    /// something every authenticated principal may do. The attachment itself is not
    /// bound to the channel afterwards (design §6.1).
    pub(crate) channel: String,
    /// The declared media type. Imposed in the signature, so an upload that lies is
    /// refused by the bucket rather than discovered later.
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
    path = "/attachments/{id}",
    tag = "notifications",
    params(("id" = String, Path, description = "The attachment id a payload references")),
    responses(
        (status = 200, description = "A short-lived signed download", body = PresignedDownload),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 404, description = "No such attachment, or none the caller may see — \
                                      deliberately the same answer", body = Problem),
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
