//! Channels and their members (design §8, §11).

use actix_web::{HttpResponse, delete, get, post, web};
use notify_domain::{Channel, ChannelId, Principal, Scope};
use notify_ports::Page;
use serde::Deserialize;
use utoipa::ToSchema;

use crate::extract::{Authenticated, page};
use crate::problem::{ApiError, Problem};
use crate::routes::notifications::PageParams;
use crate::state::ApiState;

/// Create a channel through the API. GitOps-managed ones come from config instead.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateChannelBody {
    /// The id, which is also the name in every URL and every config file.
    pub id: ChannelId,
    /// A human label, when the id is not one.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Add somebody to a channel.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AddMemberBody {
    /// Who: a user or a service account.
    pub principal: Principal,
    /// What they may do there.
    pub scopes: Vec<Scope>,
}

/// The channels the caller can see.
#[utoipa::path(
    get,
    path = "/channels",
    tag = "channels",
    params(PageParams),
    responses(
        (status = 200, description = "A page of channels", body = Page<Channel>),
        (status = 401, description = "No usable credential", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/channels")]
pub(crate) async fn list_channels(
    state: web::Data<ApiState>,
    caller: Authenticated,
    params: web::Query<PageParams>,
) -> Result<web::Json<Page<Channel>>, ApiError> {
    let params = params.into_inner();
    let page = page(params.cursor.as_deref(), params.limit)?;

    Ok(web::Json(state.channels.execute(&caller.0, page).await?))
}

/// Create a channel.
#[utoipa::path(
    post,
    path = "/channels",
    tag = "channels",
    request_body = CreateChannelBody,
    responses(
        (status = 201, description = "Created, with the caller as its first manager", body = Channel),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 409, description = "That id is taken", body = Problem),
        (status = 422, description = "The id does not validate", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/channels")]
pub(crate) async fn create_channel(
    state: web::Data<ApiState>,
    caller: Authenticated,
    body: web::Json<CreateChannelBody>,
) -> Result<HttpResponse, ApiError> {
    let body = body.into_inner();

    let channel = state
        .create_channel
        .execute(&caller.0, body.id, body.display_name.as_deref())
        .await?;

    Ok(HttpResponse::Created().json(channel))
}

/// Delete a channel. Refused on a GitOps-managed one.
#[utoipa::path(
    delete,
    path = "/channels/{channel}",
    tag = "channels",
    params(("channel" = String, Path, description = "Channel id")),
    responses(
        (status = 204, description = "Deleted, or was not there"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "No `manage` scope there", body = Problem),
        (status = 409, description = "The channel is managed; change it in GitOps", body = Problem),
    ),
    security(("bearer" = []))
)]
#[delete("/channels/{channel}")]
pub(crate) async fn delete_channel(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let channel = ChannelId::parse(&path.into_inner())?;
    state.delete_channel.execute(&caller.0, &channel).await?;

    // Idempotent by design, so a retried delete is not a 404 the client has to
    // special-case.
    Ok(HttpResponse::NoContent().finish())
}

/// Add a member.
#[utoipa::path(
    post,
    path = "/channels/{channel}/members",
    tag = "channels",
    request_body = AddMemberBody,
    params(("channel" = String, Path, description = "Channel id")),
    responses(
        (status = 200, description = "The channel as it now stands", body = Channel),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "No `manage` scope there", body = Problem),
        (status = 409, description = "The channel is managed; change it in GitOps", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/channels/{channel}/members")]
pub(crate) async fn add_member(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
    body: web::Json<AddMemberBody>,
) -> Result<web::Json<Channel>, ApiError> {
    let channel = ChannelId::parse(&path.into_inner())?;
    let body = body.into_inner();

    Ok(web::Json(
        state
            .members
            .add(&caller.0, &channel, body.principal, body.scopes)
            .await?,
    ))
}

/// Remove a member, or leave.
#[utoipa::path(
    delete,
    path = "/channels/{channel}/members/{principal}",
    tag = "channels",
    params(
        ("channel" = String, Path, description = "Channel id"),
        ("principal" = String, Path,
         description = "Principal reference, e.g. `user:usr_01J…` or `service_account:bot-argocd`"),
    ),
    responses(
        (status = 204, description = "Removed, or was not a member"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "No `manage` scope there", body = Problem),
        (status = 409, description = "The channel is managed; change it in GitOps", body = Problem),
    ),
    security(("bearer" = []))
)]
#[delete("/channels/{channel}/members/{principal}")]
pub(crate) async fn remove_member(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<(String, String)>,
) -> Result<HttpResponse, ApiError> {
    let (channel, principal) = path.into_inner();
    let channel = ChannelId::parse(&channel)?;
    let principal = crate::routes::principal_from_reference(&principal)?;

    state
        .members
        .remove(&caller.0, &channel, &principal)
        .await?;

    Ok(HttpResponse::NoContent().finish())
}
