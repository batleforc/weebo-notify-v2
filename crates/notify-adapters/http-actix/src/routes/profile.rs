//! The caller's own profile (design §2.4, §8).
//!
//! `/users/me/profile` and never `/users/{id}/profile`: §2.4 is explicit that no
//! endpoint reads somebody else's, and the path is where that decision is visible.
//! A profile reaches other people **embedded** — as the `author` of a revision, in
//! a member list, beside a receipt — which is what mechanically enforces "visible
//! only through a shared channel". Adding a lookup route later means redoing that
//! analysis rather than adding a handler.

use actix_web::{HttpResponse, get, put, web};
use notify_domain::limits::MAX_AVATAR_BYTES;
use notify_domain::{AttachmentId, DomainError, FieldEdit, Profile, ProfileEdit};
use serde::{Deserialize, Deserializer};
use utoipa::ToSchema;

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// A partial edit of the caller's profile.
///
/// Every field is doubly optional on the wire, and the two levels mean different
/// things — this is the whole reason the body is not just a `Profile`:
///
/// * **absent** — leave the field exactly as it is;
/// * **`null`** — hand the field back to the identity provider, so the next login
///   refreshes it;
/// * **a value** — claim it, and stop later logins from overwriting it (§2.4).
///
/// Without the middle case there is no way back: somebody who typed a display name
/// once would be maintaining it by hand forever.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct ProfileBody {
    /// The name shown next to everything this person publishes.
    #[serde(default, deserialize_with = "written")]
    #[schema(value_type = Option<String>)]
    pub display_name: Option<FieldEdit>,
    /// An IANA name such as `Europe/Paris`, validated against the real database.
    /// It is the default quiet hours are resolved in (§7.4).
    #[serde(default, deserialize_with = "written")]
    #[schema(value_type = Option<String>)]
    pub timezone: Option<FieldEdit>,
    /// A BCP 47 tag such as `fr-FR`.
    #[serde(default, deserialize_with = "written")]
    #[schema(value_type = Option<String>)]
    pub locale: Option<FieldEdit>,
}

/// Tell "absent" and "explicitly null" apart.
///
/// Without this, serde collapses both onto the outer `None` — `#[serde(default)]`
/// fills in the absent case and a literal `null` deserialises to the very same
/// value. The endpoint would then have no way to express "hand this field back to
/// the identity provider" (§2.4), and a `null` would silently mean "leave it as it
/// is": the opposite of what the caller asked for.
///
/// serde only calls this when the key is present, so reaching it at all means the
/// field was written — whatever it was written as.
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

/// The body of an avatar upload: the image bytes, nothing wrapping them.
///
/// Nothing reads it: the handler takes `web::Bytes` directly. It exists so the
/// document has a name for "raw bytes".
///
/// A named schema rather than an inline one because the wire type has to be
/// `string` with `format: binary` — that is how OpenAPI spells "raw bytes", and a
/// generator that reads anything else refuses the document outright rather than
/// guessing (which is what `Vec<u8>` did: an array of integers).
#[derive(Debug, ToSchema)]
#[schema(value_type = String, format = Binary)]
pub struct AvatarUpload(
    #[allow(
        dead_code,
        reason = "the handler takes `web::Bytes`; this field exists so utoipa has a \
                  type to hang the binary schema on"
    )]
    Vec<u8>,
);

/// Replace your own avatar.
///
/// The body is the image itself rather than a multipart form: there is exactly one
/// field, and a multipart parser is a parser more on a path that already decodes
/// user-supplied media (§15).
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
    body: web::Bytes,
) -> Result<web::Json<Profile>, ApiError> {
    // The ceiling is checked here as well as in the decoder, and both are wanted:
    // this one stops the bytes before they are buffered, the other one is the rule
    // the port promises whatever calls it.
    if body.len() > MAX_AVATAR_BYTES {
        return Err(DomainError::TooLarge {
            what: "avatar",
            max: MAX_AVATAR_BYTES,
            actual: body.len(),
        }
        .into());
    }

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

    Ok(HttpResponse::Ok()
        .content_type(object.content_type)
        // Immutable: replacing an avatar mints a new id (§2.4), so a cached one can
        // never be stale — and an inbox renders the same face on every row.
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
