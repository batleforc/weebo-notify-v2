//! `PUT /attachments/fs/{id}/upload` and `GET /attachments/fs/{id}/download` — the
//! filesystem attachment store's own bytes-carrying routes (ADR-0022).
//!
//! Every other attachment route in this crate hands out a credential and stays off
//! the data path (`attachments.rs`'s module doc: client → bucket, never through
//! here). This is the one adapter that cannot do that — a directory on disk has no
//! bucket-style endpoint of its own — so `FsAttachments::presign_upload` and
//! `presign_download` point back at these two routes instead, with a short-lived
//! HMAC token in the query string standing in for a bucket signature.
//!
//! Unauthenticated on purpose, the same way a presigned S3 URL is: the token *is*
//! the credential, checked by [`notify_attachments_fs::FsAttachments::verify`]
//! rather than by the `Authenticated` extractor every other handler here takes —
//! there is nothing to authenticate a bearer of a valid, unexpired token against.

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

/// The upload's body, described by hand rather than derived — the same rule
/// `notify-ports::schema` states for D1 and D3: what utoipa composes for a plain
/// `String` body is valid OpenAPI (`type: string`, no format) that progenitor
/// refuses to treat as binary, so `gen:client:rust` fails on a document nothing
/// else in this codebase would call malformed. `format: binary` is the one detail
/// a bare Rust type cannot carry, and it is the whole difference.
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

/// Why one of these two routes refused a request.
///
/// Its own type rather than [`crate::problem::ApiError`]: an invalid or expired
/// token is a client fault with no [`notify_ports::Failure`] to carry it — there is
/// no domain rule it broke and no port that failed — and folding it into
/// `PortError` would mislabel a `403` as "the world was wrong".
#[derive(Debug)]
pub(crate) enum FsError {
    /// This deployment is not running the filesystem backend.
    NotConfigured,
    /// The path segment does not parse as an attachment id.
    NotFound,
    /// The token does not check out for this id and purpose.
    Invalid(TokenError),
    /// The declared content type is not one attachments may be.
    ///
    /// Distinct from [`Self::Invalid`] on purpose: the token may be perfectly good
    /// and the type still refused, and `415` says which of the two happened. It is
    /// also checked *before* the token, so a caller learns their type is unsupported
    /// without the answer depending on whether they hold a valid credential.
    UnsupportedType,
    /// The object is not there.
    NoSuchObject,
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
            Self::Store(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn error_response(&self) -> HttpResponse {
        if let Self::Store(error) = self {
            // The one branch worth an operator's attention: the other three are a
            // client holding a bad or expired credential, which is unremarkable.
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
    body: web::Bytes,
) -> Result<HttpResponse, FsError> {
    let store = state
        .attachments_fs
        .as_ref()
        .ok_or(FsError::NotConfigured)?;
    let id: AttachmentId = id.into_inner().parse().map_err(|_| FsError::NotFound)?;

    // Normalised the same way the presign use case normalised it, by the same
    // function — comparing a raw header against a normalised signature would refuse
    // `image/png; charset=binary`, which is the honest client, while the dishonest
    // one is caught either way.
    let declared = notify_app::normalised_content_type(
        request
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
    );

    // The allowlist, applied again at the route that carries the bytes. The presign
    // already checked it — against a *different request*, which is precisely why the
    // payload ceiling is re-applied here too (`payload_config` below). A check that
    // ran on the request before this one is not a check on this one.
    if !notify_app::is_allowed_attachment_type(&declared) {
        return Err(FsError::UnsupportedType);
    }

    // The content type goes through the token's MAC, so this single call is also the
    // comparison against what the presign approved: a PUT declaring `text/html`
    // against a token signed for `image/png` fails here, exactly as it would against
    // an S3 signature.
    store
        .verify(
            Purpose::Upload {
                content_type: &declared,
            },
            id,
            &query.token,
        )
        .map_err(FsError::Invalid)?;

    // The normalised form is stored, never the raw header: what was signed is what
    // is served back.
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

/// The payload limit the upload route is registered under (design §8.2): the same
/// ceiling the presign use case already checked before signing, applied again here
/// because that check ran against a different request and this is the one that
/// actually carries the bytes.
pub(crate) fn payload_config() -> web::PayloadConfig {
    web::PayloadConfig::new(usize::try_from(MAX_ATTACHMENT_BYTES).unwrap_or(usize::MAX))
}
