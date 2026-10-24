//! The caller's own profile (design §2.4, §8). Only `/users/me`: other people's profiles
//! are only ever seen embedded (author, member list, receipt).

use actix_web::{HttpRequest, HttpResponse, get, put, web};
use notify_domain::limits::MAX_AVATAR_BYTES;
use notify_domain::{AttachmentId, DomainError, FieldEdit, Profile, ProfileEdit};
use serde::{Deserialize, Deserializer};
use utoipa::ToSchema;

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// A partial edit of the caller's profile: an absent field is left alone, `null` hands it
/// back to the identity provider, a value claims it against later logins (§2.4).
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct ProfileBody {
    /// The name shown next to everything this person publishes.
    #[serde(default, deserialize_with = "written")]
    #[schema(value_type = Option<String>)]
    pub display_name: Option<FieldEdit>,
    /// An IANA name such as `Europe/Paris`; the default timezone for quiet hours (§7.4).
    #[serde(default, deserialize_with = "written")]
    #[schema(value_type = Option<String>)]
    pub timezone: Option<FieldEdit>,
    /// A BCP 47 tag such as `fr-FR`.
    #[serde(default, deserialize_with = "written")]
    #[schema(value_type = Option<String>)]
    pub locale: Option<FieldEdit>,
}

/// Tell "absent" from "explicitly null": serde only calls this when the key is present.
fn written<'de, D>(deserializer: D) -> Result<Option<FieldEdit>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(Some(value.map_or(FieldEdit::Follow, FieldEdit::Set)))
}

impl From<ProfileBody> for ProfileEdit {
    fn from(body: ProfileBody) -> Self {
        Self {
            display_name: body.display_name,
            timezone: body.timezone,
            locale: body.locale,
        }
    }
}

/// Read your own profile.
#[utoipa::path(
    get,
    path = "/users/me/profile",
    tag = "identity",
    responses(
        (status = 200, description = "The caller's profile", body = Profile),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "This principal is not a person (design §2.4)", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/users/me/profile")]
pub(crate) async fn read_profile(
    state: web::Data<ApiState>,
    caller: Authenticated,
) -> Result<web::Json<Profile>, ApiError> {
    Ok(web::Json(state.profile.execute(&caller.0).await?))
}

/// The body of an avatar upload: the raw image bytes (`format: binary`).
#[derive(Debug, ToSchema)]
#[schema(value_type = String, format = Binary)]
pub struct AvatarUpload(
    #[expect(
        dead_code,
        reason = "the handler streams `web::Payload`; this field exists so utoipa has a \
                  type to hang the binary schema on"
    )]
    Vec<u8>,
);

/// Replace your own avatar; the body is the image itself, not a multipart form.
#[utoipa::path(
    put,
    path = "/users/me/avatar",
    tag = "identity",
    request_body(
        content = AvatarUpload,
        description = "PNG, JPEG or WebP, at most 512 KiB",
        content_type = "application/octet-stream",
    ),
    responses(
        (status = 200, description = "The profile, now naming the stored avatar", body = Profile),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "This principal is not a person (design §2.4)", body = Problem),
        (status = 413, description = "Over the 512 KiB ceiling", body = Problem),
        (status = 422, description = "Not a supported image, or one that will not decode", body = Problem),
    ),
    security(("bearer" = []))
)]
#[put("/users/me/avatar")]
pub(crate) async fn set_avatar(
    state: web::Data<ApiState>,
    caller: Authenticated,
    request: HttpRequest,
    payload: web::Payload,
) -> Result<web::Json<Profile>, ApiError> {
    // Checked here too so reading stops as soon as the body passes the ceiling.
    let body = match payload.to_bytes_limited(MAX_AVATAR_BYTES).await {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => {
            return Err(DomainError::Malformed {
                field: "avatar",
                reason: "the upload did not complete",
            }
            .into());
        }
        Err(_) => {
            // Stopped early, so the real size is only known if the client said it.
            let declared = request
                .headers()
                .get("content-length")
                .and_then(|value| value.to_str().ok()?.parse().ok());
            return Err(DomainError::TooLarge {
                what: "avatar",
                max: MAX_AVATAR_BYTES,
                actual: declared.unwrap_or(MAX_AVATAR_BYTES + 1),
            }
            .into());
        }
    };

    Ok(web::Json(state.set_avatar.execute(&caller.0, &body).await?))
}

/// Fetch an avatar by the id a profile or an author gave you.
#[utoipa::path(
    get,
    path = "/avatars/{id}",
    tag = "identity",
    params(("id" = String, Path, description = "The attachment id from a profile")),
    responses(
        (status = 200, description = "The image", content_type = "image/png"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 422, description = "No such object (`field.malformed`)", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/avatars/{id}")]
pub(crate) async fn read_avatar(
    state: web::Data<ApiState>,
    caller: Authenticated,
    id: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let id = id.into_inner().parse::<AttachmentId>()?;

    let Some(object) = state.avatar.execute(&caller.0, id).await? else {
        return Err(DomainError::Malformed {
            field: "avatar",
            reason: "no such object",
        }
        .into());
    };

    Ok(crate::routes::served_bytes(&mut HttpResponse::Ok())
        .content_type(object.content_type)
        // Immutable: replacing an avatar mints a new id (§2.4).
        .insert_header(("cache-control", "private, max-age=86400, immutable"))
        .body(object.bytes))
}

/// Edit your own profile.
#[utoipa::path(
    put,
    path = "/users/me/profile",
    tag = "identity",
    request_body = ProfileBody,
    responses(
        (status = 200, description = "The profile as it now stands", body = Profile),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "This principal is not a person (design §2.4)", body = Problem),
        (status = 422, description = "A timezone the IANA database does not know, or text that fails hygiene", body = Problem),
    ),
    security(("bearer" = []))
)]
#[put("/users/me/profile")]
pub(crate) async fn update_profile(
    state: web::Data<ApiState>,
    caller: Authenticated,
    body: web::Json<ProfileBody>,
) -> Result<web::Json<Profile>, ApiError> {
    let edit = ProfileEdit::from(body.into_inner());

    Ok(web::Json(
        state.update_profile.execute(&caller.0, edit).await?,
    ))
}
