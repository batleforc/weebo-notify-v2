//! The caller's own do-not-disturb (design §7.4, §8). No subject in the path, so it
//! cannot be asked about anyone else; `PUT` replaces it whole with a `DndDraft`.

use actix_web::{get, put, web};
use notify_domain::{Dnd, DndDraft};

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// Read your own do-not-disturb.
#[utoipa::path(
    get,
    path = "/preferences/dnd",
    tag = "identity",
    responses(
        (status = 200, description = "The caller's do-not-disturb; muting nothing if never set", body = Dnd),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "This principal is not a person (design §5.2bis)", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/preferences/dnd")]
pub(crate) async fn read_dnd(
    state: web::Data<ApiState>,
    caller: Authenticated,
) -> Result<web::Json<Dnd>, ApiError> {
    Ok(web::Json(state.dnd.execute(&caller.0).await?))
}

/// Replace your own do-not-disturb.
#[utoipa::path(
    put,
    path = "/preferences/dnd",
    tag = "identity",
    request_body = DndDraft,
    responses(
        (status = 200, description = "The stored preference", body = Dnd),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "This principal is not a person (design §5.2bis)", body = Problem),
        (status = 422, description = "A window that could not be resolved — an unknown timezone, or ends that are equal", body = Problem),
    ),
    security(("bearer" = []))
)]
#[put("/preferences/dnd")]
pub(crate) async fn set_dnd(
    state: web::Data<ApiState>,
    caller: Authenticated,
    body: web::Json<DndDraft>,
) -> Result<web::Json<Dnd>, ApiError> {
    Ok(web::Json(
        state.set_dnd.execute(&caller.0, body.into_inner()).await?,
    ))
}
