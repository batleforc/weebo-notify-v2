//! `POST /auth/token/exchange` — the CI federation door (design §5.3, §8).
//!
//! The only route under `/api/v1` that takes **no** credential of ours, because it
//! is where a credential is obtained. What stands in for authentication is the
//! forge's signature over the body's token, and the three refusals scenario T11
//! names, all of which happen behind [`ExchangeToken`].
//!
//! [`ExchangeToken`]: notify_app::ExchangeToken

use actix_web::{post, web};
use notify_ports::MintedToken;
use serde::Deserialize;
use utoipa::ToSchema;

use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// The forge's ID token, presented for exchange.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ExchangeBody {
    /// The OIDC ID token the CI runner was given by its forge.
    ///
    /// In the body rather than in `Authorization`, deliberately: it is not a
    /// credential for *this* API — sending it as a bearer would invite a proxy or a
    /// log to treat it as one, and would make this the one endpoint where the
    /// header means something different.
    pub(crate) token: String,
}

/// Exchange a forge ID token for a short-lived weebo-notify token.
#[utoipa::path(
    post,
    path = "/auth/token/exchange",
    tag = "auth",
    request_body = ExchangeBody,
    responses(
        (status = 200, description = "A token valid for at most 15 minutes", body = MintedToken),
        (status = 401, description = "Unverifiable, replayed, or matched by no policy", body = Problem),
        (status = 429, description = "Over the per-(issuer, subject) budget", body = Problem),
        (status = 501, description = "This deployment does not federate", body = Problem),
    )
)]
#[post("/auth/token/exchange")]
pub(crate) async fn exchange(
    state: web::Data<ApiState>,
    body: web::Json<ExchangeBody>,
) -> Result<web::Json<MintedToken>, ApiError> {
    // A deployment with no `[federation]` block says so, rather than refusing every
    // token as invalid: a pipeline whose credentials are fine deserves to be told
    // the server does not do this, and `NotConfigured` is the table §14.3 already
    // uses for every absent capability.
    let exchange = state
        .exchange
        .as_ref()
        .ok_or(notify_ports::PortError::NotConfigured { what: "federation" })?;

    Ok(web::Json(exchange.execute(&body.into_inner().token).await?))
}
