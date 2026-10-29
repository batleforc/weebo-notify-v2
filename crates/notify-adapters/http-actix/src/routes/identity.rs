//! Devices and API keys (design §5.2, §8).

use actix_web::{HttpResponse, delete, get, patch, post, web};
use notify_domain::{
    ApiKey, ApiKeyId, ApiKeyOwner, ChannelId, Device, DeviceId, DevicePublicKey, DomainError,
    Principal, PushTransport, Scope, UserId,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::extract::{Authenticated, KeyAdministrator};
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// The caller's own id, to name itself as `owner` of e.g. a personal API key (§5.2).
#[derive(Debug, Serialize, ToSchema)]
pub struct Whoami {
    /// The caller's own id.
    pub id: UserId,
}

/// Who the caller is; users only, since keys act for someone else (design §5.2bis).
#[utoipa::path(
    get,
    path = "/me",
    tag = "identity",
    responses(
        (status = 200, description = "The caller's own id", body = Whoami),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "This principal has no identity of its own (design §5.2bis)", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/me")]
pub(crate) async fn whoami(caller: Authenticated) -> Result<web::Json<Whoami>, ApiError> {
    let Principal::User { id } = caller.0.principal else {
        return Err(DomainError::NotAPerson.into());
    };

    Ok(web::Json(Whoami { id }))
}

/// Enrol a device.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EnrolBody {
    /// What to call it in the device list.
    pub label: String,
    /// How it is reached. `stream` is a device that only holds a connection.
    pub transport: PushTransport,
    /// The X25519 public key; required for relayed transports, refused otherwise (§6.2).
    #[serde(default)]
    pub public_key: Option<DevicePublicKey>,
}

/// Mint an API key.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateKeyBody {
    /// Who the key acts as: a user, a service account, or the channel itself.
    pub owner: ApiKeyOwner,
    /// A label, so a key list is readable a year later.
    #[serde(default)]
    pub label: Option<String>,
    /// What it may do.
    pub scopes: Vec<Scope>,
    /// Where it may do it.
    pub channels: Vec<ChannelId>,
    /// When it stops working; absent means never.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

/// A newly minted key: the only response that ever carries its secret (T12).
#[derive(Debug, Serialize, ToSchema)]
pub struct MintedKey {
    /// The key's metadata, exactly as `GET /apikeys` will show it from now on.
    #[serde(flatten)]
    pub key: ApiKey,
    /// `wnk_<prefix>_<secret>`. Store it now; no endpoint can show it again.
    #[schema(example = "wnk_a1b2c3d4_…")]
    pub secret: String,
}

/// Enrol a device.
#[utoipa::path(
    post,
    path = "/devices",
    tag = "devices",
    request_body = EnrolBody,
    responses(
        (status = 201, description = "Enrolled", body = Device),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "A push transport, from a credential that is not a session", body = Problem),
        (status = 422, description = "A relayed transport with no key, or the reverse; or `device.limit_reached`, the owner's `[devices] max_per_user` active devices already enrolled", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/devices")]
pub(crate) async fn enrol_device(
    state: web::Data<ApiState>,
    caller: Authenticated,
    body: web::Json<EnrolBody>,
) -> Result<HttpResponse, ApiError> {
    let body = body.into_inner();

    let device = state
        .enrol_device
        .execute(&caller.0, &body.label, body.transport, body.public_key)
        .await?;

    Ok(HttpResponse::Created().json(device))
}

/// The caller's devices.
#[utoipa::path(
    get,
    path = "/devices",
    tag = "devices",
    responses(
        (status = 200, description = "Devices, newest first — `stream` ones only for an API key or a CI token", body = Vec<Device>),
        (status = 401, description = "No usable credential", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/devices")]
pub(crate) async fn list_devices(
    state: web::Data<ApiState>,
    caller: Authenticated,
) -> Result<web::Json<Vec<Device>>, ApiError> {
    Ok(web::Json(state.devices.execute(&caller.0).await?))
}

/// Revoke a device: its push subscription and its sync cursor go with it.
#[utoipa::path(
    delete,
    path = "/devices/{id}",
    tag = "devices",
    params(("id" = String, Path, description = "Device id")),
    responses(
        (status = 204, description = "Revoked, or was not there"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "A push device, from a credential that is not a session", body = Problem),
    ),
    security(("bearer" = []))
)]
#[delete("/devices/{id}")]
pub(crate) async fn revoke_device(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let id: DeviceId = path.into_inner().parse()?;
    state.revoke_device.execute(&caller.0, id).await?;
    Ok(HttpResponse::NoContent().finish())
}

/// Mint an API key. The secret comes back once.
#[utoipa::path(
    post,
    path = "/apikeys",
    tag = "auth",
    request_body = CreateKeyBody,
    responses(
        (status = 201, description = "Minted — store the secret now", body = MintedKey),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not entitled to mint for that owner or channel", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/apikeys")]
pub(crate) async fn create_key(
    state: web::Data<ApiState>,
    // The one place a key issuer is admitted (ADR-0033).
    caller: KeyAdministrator,
    body: web::Json<CreateKeyBody>,
) -> Result<HttpResponse, ApiError> {
    let body = body.into_inner();

    let minted = state
        .create_key
        .execute(
            &caller.0,
            notify_app::NewApiKey {
                owner: body.owner,
                label: body.label,
                scopes: body.scopes,
                channels: body.channels,
                expires_at: body.expires_at,
            },
        )
        .await?;

    Ok(HttpResponse::Created().json(MintedKey {
        key: minted.key,
        secret: minted.secret,
    }))
}

/// The caller's keys — metadata only, never a secret.
#[utoipa::path(
    get,
    path = "/apikeys",
    tag = "auth",
    responses(
        (status = 200, description = "Keys, with prefix, scopes and last use", body = Vec<ApiKey>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a session — a key cannot list keys", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/apikeys")]
pub(crate) async fn list_keys(
    state: web::Data<ApiState>,
    // The one place a key issuer is admitted (ADR-0033).
    caller: KeyAdministrator,
) -> Result<web::Json<Vec<ApiKey>>, ApiError> {
    Ok(web::Json(state.keys.execute(&caller.0).await?))
}

/// Revoke a key, effective on the next request that presents it.
#[utoipa::path(
    delete,
    path = "/apikeys/{id}",
    tag = "auth",
    params(("id" = String, Path, description = "API key id")),
    responses(
        (status = 204, description = "Revoked, or was not there"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a session — a key may revoke only itself", body = Problem),
    ),
    security(("bearer" = []))
)]
#[delete("/apikeys/{id}")]
pub(crate) async fn revoke_key(
    state: web::Data<ApiState>,
    // The one place a key issuer is admitted (ADR-0033).
    caller: KeyAdministrator,
    path: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let id: ApiKeyId = path.into_inner().parse()?;
    state.revoke_key.execute(&caller.0, id).await?;
    Ok(HttpResponse::NoContent().finish())
}

/// Narrow (or clear) one key's own rate-limit ceiling.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ApiKeyLimitBody {
    /// The new ceiling, or `null` to fall back to the channel's own (design §8.2).
    #[serde(default)]
    pub publish_per_minute: Option<u32>,
}

/// Set a platform-admin's ceiling on one key's own publish rate (design §8.2, D24);
/// not open to the key's owner.
#[utoipa::path(
    patch,
    path = "/apikeys/{id}/limit",
    tag = "auth",
    params(("id" = String, Path, description = "API key id")),
    request_body = ApiKeyLimitBody,
    responses(
        (status = 200, description = "The key, with its new ceiling", body = ApiKey),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a platform admin", body = Problem),
        (status = 422, description = "No such key (`field.malformed`)", body = Problem),
    ),
    security(("bearer" = []))
)]
#[patch("/apikeys/{id}/limit")]
pub(crate) async fn set_key_limit(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
    body: web::Json<ApiKeyLimitBody>,
) -> Result<web::Json<ApiKey>, ApiError> {
    let id: ApiKeyId = path.into_inner().parse()?;

    let Some(key) = state
        .set_key_limit
        .execute(&caller.0, id, body.into_inner().publish_per_minute)
        .await?
    else {
        return Err(notify_domain::DomainError::Malformed {
            field: "id",
            reason: "no such key",
        }
        .into());
    };

    Ok(web::Json(key))
}
