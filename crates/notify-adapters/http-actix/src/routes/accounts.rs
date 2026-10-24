//! Service accounts (design §5.2bis, §8). `DELETE` deactivates: the row stays readable in
//! history and audit, and its keys are revoked in the same call (T25).

use actix_web::{delete, get, post, web};
use notify_domain::{ServiceAccount, ServiceAccountId, User};
use notify_ports::Page;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::extract::{Authenticated, page};
use crate::problem::{ApiError, Problem};
use crate::routes::notifications::PageParams;
use crate::state::ApiState;

/// What it takes to create one.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct CreateAccountBody {
    /// The name it will be known by everywhere (author, audit log, member list).
    pub id: String,
    /// What clients show instead of the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// What it is for, for whoever finds it in an audit log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A person, as the member picker shows them.
#[derive(Debug, Serialize, ToSchema)]
pub struct UserCard {
    /// Their id, which `POST /channels/{channel}/members` takes.
    pub id: String,
    /// Their display name, when known.
    pub display_name: Option<String>,
    /// Their email, when the identity provider released one.
    pub email: Option<String>,
}

impl From<User> for UserCard {
    fn from(user: User) -> Self {
        Self {
            id: user.id.to_string(),
            display_name: user.display_name,
            email: user.email,
        }
    }
}

/// What to look for.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UserQuery {
    /// Part of a name or an email; at least two characters.
    pub q: String,
}

/// Find people to add to a channel (at most 10). Not available to API keys of bots.
#[utoipa::path(
    get,
    path = "/users",
    tag = "identity",
    params(UserQuery),
    responses(
        (status = 200, description = "The matches", body = Vec<UserCard>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a person", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/users")]
pub(crate) async fn search_users(
    state: web::Data<ApiState>,
    caller: Authenticated,
    query: web::Query<UserQuery>,
) -> Result<web::Json<Vec<UserCard>>, ApiError> {
    let users = state.users.execute(&caller.0, &query.q).await?;
    Ok(web::Json(users.into_iter().map(UserCard::from).collect()))
}

/// Every service account on this instance.
#[utoipa::path(
    get,
    path = "/serviceaccounts",
    tag = "identity",
    params(PageParams),
    responses(
        (status = 200, description = "A page of accounts, deactivated ones included", body = Page<ServiceAccount>),
        (status = 401, description = "No usable credential", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/serviceaccounts")]
pub(crate) async fn list_accounts(
    state: web::Data<ApiState>,
    caller: Authenticated,
    params: web::Query<PageParams>,
) -> Result<web::Json<Page<ServiceAccount>>, ApiError> {
    let params = params.into_inner();
    let page = page(params.cursor.as_deref(), params.limit)?;
    Ok(web::Json(state.accounts.execute(&caller.0, page).await?))
}

/// Create one, owned by the caller — a platform admin (§5.4).
#[utoipa::path(
    post,
    path = "/serviceaccounts",
    tag = "identity",
    request_body = CreateAccountBody,
    responses(
        (status = 201, description = "Created", body = ServiceAccount),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a platform admin, or not a person (design §5.2bis, §5.4)", body = Problem),
        (status = 422, description = "A name that is not a slug, or one already taken", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/serviceaccounts")]
pub(crate) async fn create_account(
    state: web::Data<ApiState>,
    caller: Authenticated,
    body: web::Json<CreateAccountBody>,
) -> Result<actix_web::HttpResponse, ApiError> {
    let body = body.into_inner();

    let account = state
        .create_account
        .execute(
            &caller.0,
            &body.id,
            body.display_name.as_deref(),
            body.description.as_deref(),
        )
        .await?;

    Ok(actix_web::HttpResponse::Created().json(account))
}

/// Deactivate one: its keys are revoked with it.
#[utoipa::path(
    delete,
    path = "/serviceaccounts/{id}",
    tag = "identity",
    params(("id" = String, Path, description = "The account's name")),
    responses(
        (status = 204, description = "Deactivated, or already was"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Somebody else's, or not a person's own session", body = Problem),
        (status = 422, description = "Managed by GitOps — change it in config (design §11)", body = Problem),
    ),
    security(("bearer" = []))
)]
#[delete("/serviceaccounts/{id}")]
pub(crate) async fn deactivate_account(
    state: web::Data<ApiState>,
    caller: Authenticated,
    id: web::Path<String>,
) -> Result<actix_web::HttpResponse, ApiError> {
    let id = ServiceAccountId::parse(&id.into_inner())?;

    state.deactivate_account.execute(&caller.0, &id).await?;

    Ok(actix_web::HttpResponse::NoContent().finish())
}
