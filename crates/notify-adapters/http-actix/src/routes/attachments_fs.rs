//! The filesystem store's own upload/download routes (ADR-0022). Unauthenticated like a
//! presigned S3 URL: the short-lived HMAC token in the query *is* the credential.

use actix_web::http::StatusCode;
use actix_web::{HttpRequest, HttpResponse, ResponseError, get, put, web};
use notify_attachments_fs::{Purpose, TokenError};
use notify_domain::AttachmentId;
use notify_domain::limits::MAX_ATTACHMENT_BYTES;
use notify_ports::{AttachmentStore as _, PortError};
use serde::Deserialize;
use utoipa::openapi::{KnownFormat, Object, RefOr, Schema, SchemaFormat, Type};
use utoipa::{PartialSchema, ToSchema};

use crate::state::ApiState;

#[derive(Debug, Deserialize)]
pub(crate) struct TokenQuery {
    token: String,
}

/// The upload body, hand-described as `format: binary` so progenitor treats it as bytes.
struct RawBytes;

impl PartialSchema for RawBytes {
    fn schema() -> RefOr<Schema> {
        Object::builder()
            .schema_type(Type::String)
            .format(Some(SchemaFormat::KnownFormat(KnownFormat::Binary)))
            .description(Some("The raw attachment bytes."))
            .build()
            .into()
    }
}

impl ToSchema for RawBytes {}

/// Why one of these two routes refused; not an `ApiError`, since a bad token breaks no
/// domain rule and no port failed.
#[derive(Debug)]
pub(crate) enum FsError {
    /// This deployment is not running the filesystem backend.
    NotConfigured,
    /// The path segment does not parse as an attachment id.
    NotFound,
    /// The token does not check out for this id and purpose.
    Invalid(TokenError),
    /// The declared content type is not allowlisted (`415`); checked before the token.
    UnsupportedType,
    /// The object is not there.
    NoSuchObject,
    /// An upload token is good for one upload: the object is already there.
    AlreadyUploaded,
    /// Over the ceiling the presign was checked against.
    TooLarge,
    /// The client stopped sending halfway.
    Aborted,
    /// The store itself failed.
    Store(PortError),
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotConfigured => {
                write!(f, "this deployment has no filesystem attachment store")
            }
            Self::NotFound | Self::NoSuchObject => write!(f, "no such attachment"),
            Self::Invalid(TokenError::Expired) => write!(f, "the token has expired"),
            Self::Invalid(TokenError::Malformed) => write!(f, "the token is malformed"),
            Self::Invalid(TokenError::Mismatch) => {
                write!(f, "the token does not match this attachment")
            }
            Self::UnsupportedType => write!(
                f,
                "attachments are images: png, jpeg, webp or gif (design §2.3)"
            ),
            Self::AlreadyUploaded => write!(f, "this attachment has already been uploaded"),
            Self::TooLarge => write!(f, "attachments are at most 25 MiB (design §8.2)"),
            Self::Aborted => write!(f, "the upload did not complete"),
            Self::Store(_) => write!(f, "the request could not be served"),
        }
    }
}

impl From<PortError> for FsError {
    fn from(error: PortError) -> Self {
        Self::Store(error)
    }
}

impl ResponseError for FsError {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::NotConfigured => StatusCode::NOT_IMPLEMENTED,
            Self::NotFound | Self::NoSuchObject => StatusCode::NOT_FOUND,
            Self::Invalid(_) => StatusCode::FORBIDDEN,
            Self::UnsupportedType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::AlreadyUploaded => StatusCode::CONFLICT,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Aborted => StatusCode::BAD_REQUEST,
            Self::Store(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn error_response(&self) -> HttpResponse {
        if let Self::Store(error) = self {
            // The only branch worth an operator's attention.
            tracing::error!(%error, "the filesystem store refused a bytes-carrying request");
        }
        HttpResponse::build(self.status_code()).body(self.to_string())
    }
}

/// Accept the bytes a `presign_upload` URL was signed for.
#[utoipa::path(
    put,
    path = "/attachments/fs/{id}/upload",
    tag = "notifications",
    params(
        ("id" = String, Path, description = "The attachment id `presign_upload` minted"),
        ("token" = String, Query, description = "The bearer token from the presigned URL"),
    ),
    request_body(content = RawBytes, content_type = "application/octet-stream", description = "The raw bytes"),
    responses(
        (status = 204, description = "Stored"),
        (status = 403, description = "The token is missing, expired, or for another id/purpose/content type"),
        (status = 404, description = "Not a valid attachment id"),
        (status = 409, description = "Already uploaded — a token is good for one upload"),
        (status = 413, description = "Over the 25 MiB ceiling (§8.2)"),
        (status = 415, description = "The declared content type is not an allowed attachment type"),
        (status = 501, description = "This deployment is not running the filesystem backend"),
    ),
)]
#[put("/attachments/fs/{id}/upload")]
pub(crate) async fn fs_upload(
    state: web::Data<ApiState>,
    id: web::Path<String>,
    query: web::Query<TokenQuery>,
    request: HttpRequest,
    // A stream, not `web::Bytes`: nothing is buffered until the token checks out.
    payload: web::Payload,
) -> Result<HttpResponse, FsError> {
    let store = state
        .attachments_fs
        .as_ref()
        .ok_or(FsError::NotConfigured)?;
    let id: AttachmentId = id.into_inner().parse().map_err(|_| FsError::NotFound)?;

    // Normalised exactly as the presign did, so `image/png; charset=binary` matches.
    let declared = notify_app::normalised_content_type(
        request
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
    );

    // Re-check the allowlist and ceiling: the presign checked a different request.
    if !notify_app::is_allowed_attachment_type(&declared) {
        return Err(FsError::UnsupportedType);
    }

    // The content type is in the token's MAC, so this also compares it to the presign.
    store
        .verify(
            Purpose::Upload {
                content_type: &declared,
            },
            id,
            &query.token,
        )
        .map_err(FsError::Invalid)?;

    // One upload per token (atomic claim), so bytes cannot be swapped after publishing.
    if !store.claim_upload(id).await? {
        return Err(FsError::AlreadyUploaded);
    }

    let limit = usize::try_from(MAX_ATTACHMENT_BYTES).unwrap_or(usize::MAX);
    let body = match payload.to_bytes_limited(limit).await {
        Ok(Ok(body)) => body,
        failed => {
            // Nothing landed: give the token its one upload back.
            store.delete(id).await?;
            return Err(if failed.is_err() {
                FsError::TooLarge
            } else {
                FsError::Aborted
            });
        }
    };

    // Store the normalised (signed) type, never the raw header.
    store.put(id, &declared, body.to_vec()).await?;

    Ok(HttpResponse::NoContent().finish())
}

/// Hand back the bytes a `presign_download` URL was signed for.
#[utoipa::path(
    get,
    path = "/attachments/fs/{id}/download",
    tag = "notifications",
    params(
        ("id" = String, Path, description = "The attachment id `presign_download` minted"),
        ("token" = String, Query, description = "The bearer token from the presigned URL"),
    ),
    responses(
        (status = 200, description = "The bytes"),
        (status = 403, description = "The token is missing, expired, or for another id/purpose"),
        (status = 404, description = "No such attachment, or not a valid attachment id"),
        (status = 501, description = "This deployment is not running the filesystem backend"),
    ),
)]
#[get("/attachments/fs/{id}/download")]
pub(crate) async fn fs_download(
    state: web::Data<ApiState>,
    id: web::Path<String>,
    query: web::Query<TokenQuery>,
) -> Result<HttpResponse, FsError> {
    let store = state
        .attachments_fs
        .as_ref()
        .ok_or(FsError::NotConfigured)?;
    let id: AttachmentId = id.into_inner().parse().map_err(|_| FsError::NotFound)?;

    store
        .verify(Purpose::Download, id, &query.token)
        .map_err(FsError::Invalid)?;

    let object = store.get(id).await?.ok_or(FsError::NoSuchObject)?;

    Ok(crate::routes::served_bytes(&mut HttpResponse::Ok())
        .content_type(object.content_type)
        .body(object.bytes))
}
