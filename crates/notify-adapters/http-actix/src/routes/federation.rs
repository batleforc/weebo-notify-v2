//! `POST /auth/token/exchange`, the CI federation door (design §5.3, §8): the only route
//! taking no credential of ours; the forge's signature is checked in `ExchangeToken`.

use actix_web::{post, web};
use notify_ports::MintedToken;
use serde::Deserialize;
use utoipa::ToSchema;

use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// The forge's ID token, presented for exchange.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ExchangeBody {
    /// The OIDC ID token the CI runner got from its forge; in the body, since it is not a
    /// credential for this API.
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
    // Without `[federation]`, answer `501` rather than calling every token invalid.
    let exchange = state
        .exchange
        .as_ref()
        .ok_or(notify_ports::PortError::NotConfigured { what: "federation" })?;

    Ok(web::Json(exchange.execute(&body.into_inner().token).await?))
}
