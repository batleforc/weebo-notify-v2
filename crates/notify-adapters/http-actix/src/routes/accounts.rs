//! Service accounts (design §5.2bis, §8).
//!
//! `DELETE` deactivates rather than deletes, and the verb is the honest one: the
//! row survives so every revision and audit entry naming it stays readable. What
//! stops is its ability to act, immediately — its keys go in the same call
//! (scenario T25).

use actix_web::{delete, get, post, web};
use notify_domain::{ServiceAccount, ServiceAccountId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// What it takes to create one.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct CreateAccountBody {
    /// The name it will be known by, everywhere it appears — as an `author`, in the
    /// audit log, in a member list. Chosen rather than generated for exactly that
    /// reason (§5.2bis).
    pub id: String,
    /// What clients show instead of the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// What it is for, read by whoever finds it in an audit log in six months.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Every service account on this instance.
#[utoipa::path(
    get,
    path = "/serviceaccounts",
    tag = "identity",
    responses(
        (status = 200, description = "The accounts, deactivated ones included", body = Vec<ServiceAccount>),
        (status = 401, description = "No usable credential", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/serviceaccounts")]
pub(crate) async fn list_accounts(
    state: web::Data<ApiState>,
    caller: Authenticated,
) -> Result<web::Json<Vec<ServiceAccount>>, ApiError> {
    Ok(web::Json(state.accounts.execute(&caller.0).await?))
}

/// Create one, owned by the caller.
#[utoipa::path(
    post,
    path = "/serviceaccounts",
    tag = "identity",
    request_body = CreateAccountBody,
    responses(
        (status = 201, description = "Created", body = ServiceAccount),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "A bot cannot create bots (design §5.2bis)", body = Problem),
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
        (status = 403, description = "Somebody else's, or a person is not asking", body = Problem),
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
