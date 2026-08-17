//! The caller's own do-not-disturb (design §7.4, §8).
//!
//! `/preferences/dnd` with no subject in the path, for the same reason
//! `/users/me/profile` has none (§2.4): there is no route here that could be asked
//! about somebody else, so "a preference is private" is a property of the URL space
//! rather than a check in a handler.
//!
//! The `PUT` replaces the preference whole. Quiet hours are a list, and a partial
//! update of a list has no unambiguous meaning — a body that omitted a window would
//! be indistinguishable from one asking to drop it.
//!
//! # The body is a draft, and the response is the entity
//!
//! [`Dnd`] carries the `user` it belongs to, because the `preference` event of §8.1
//! has to say whose it is. [`DndDraft`] does not, and that is the whole difference:
//! the subject is the caller. Reusing `Dnd` as the request body was tried and it is
//! worse than merely misleading — `user` is *required*, so a client that legitimately
//! does not know its own id could not build a body at all, and one that guessed got
//! a deserialisation error about a field the server was going to overwrite anyway.

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
