//! What comes back from a notification: interactions, silences, receipts.

use actix_web::{get, post, web};
use notify_domain::{
    DeviceId, HumanDuration, Interaction, InteractionRecord, NotificationId, Readership, Receipt,
    ReceiptKind, Recorded, Silence, SilenceScope,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// What recording an interaction did: whether anything changed worth broadcasting.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecordedInteraction {
    /// `created`, `unchanged` or `replaced`.
    #[schema(example = "created")]
    pub outcome: &'static str,
    /// The interaction that now stands; absent on `unchanged`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<InteractionRecord>,
}

impl From<Recorded> for RecordedInteraction {
    fn from(recorded: Recorded) -> Self {
        let outcome = match &recorded {
            Recorded::Created(_) => "created",
            Recorded::Unchanged(_) => "unchanged",
            Recorded::Replaced { .. } => "replaced",
        };
        Self {
            outcome,
            record: recorded.record().cloned(),
        }
    }
}

/// Stop notifying *me* about this.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SilenceBody {
    /// How far it reaches. Defaults to this notification alone.
    #[serde(default)]
    pub scope: SilenceScope,
    /// How long, e.g. `2h`. A group-scoped silence must say.
    #[serde(default)]
    pub duration: Option<HumanDuration>,
}

/// Delivered, or seen.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ReceiptBody {
    /// Which notification.
    pub notification_id: NotificationId,
    /// `delivered` or `seen`.
    pub kind: ReceiptKind,
    /// Which device saw it, when the client is one.
    #[serde(default)]
    pub device: Option<DeviceId>,
}

/// Record an interaction: an acknowledgement, a reply, a choice.
#[utoipa::path(
    post,
    path = "/notifications/{id}/interactions",
    tag = "notifications",
    request_body = Interaction,
    params(("id" = String, Path, description = "Notification id")),
    responses(
        (status = 200, description = "Recorded, or already was", body = RecordedInteraction),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a member of its channel", body = Problem),
        (status = 422, description = "No such action, or the wrong kind for it", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/notifications/{id}/interactions")]
pub(crate) async fn record(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
    body: web::Json<Interaction>,
) -> Result<web::Json<RecordedInteraction>, ApiError> {
    let id: NotificationId = path.into_inner().parse()?;

    let recorded = state
        .record_interaction
        .execute(&caller.0, id, body.into_inner())
        .await?;

    // Always 200: clicking `ack` twice is one interaction.
    Ok(web::Json(recorded.into()))
}

/// Everything recorded against a notification.
#[utoipa::path(
    get,
    path = "/notifications/{id}/interactions",
    tag = "notifications",
    params(("id" = String, Path, description = "Notification id")),
    responses(
        (status = 200, description = "Interactions, oldest first", body = Vec<InteractionRecord>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a member of its channel", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/notifications/{id}/interactions")]
pub(crate) async fn list_interactions(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
) -> Result<web::Json<Vec<InteractionRecord>>, ApiError> {
    let id: NotificationId = path.into_inner().parse()?;
    Ok(web::Json(state.interactions.execute(&caller.0, id).await?))
}

/// Silence a notification, or its whole group, for the calling user.
#[utoipa::path(
    post,
    path = "/notifications/{id}/silence",
    tag = "notifications",
    request_body = SilenceBody,
    params(("id" = String, Path, description = "Notification id")),
    responses(
        (status = 200, description = "The silence now in force", body = Silence),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a member of its channel", body = Problem),
        (status = 422, description = "A group silence with no duration", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/notifications/{id}/silence")]
pub(crate) async fn silence(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
    body: web::Json<SilenceBody>,
) -> Result<web::Json<Silence>, ApiError> {
    let id: NotificationId = path.into_inner().parse()?;
    let body = body.into_inner();

    Ok(web::Json(
        state
            .silence
            .execute(&caller.0, id, body.scope, body.duration)
            .await?,
    ))
}

/// Post a delivery or read receipt.
#[utoipa::path(
    post,
    path = "/receipts",
    tag = "notifications",
    request_body = ReceiptBody,
    responses(
        (status = 200,
         description = "The receipt, or `null` when receipts are off on that channel",
         body = Option<Receipt>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a member of its channel", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/receipts")]
pub(crate) async fn receipt(
    state: web::Data<ApiState>,
    caller: Authenticated,
    body: web::Json<ReceiptBody>,
) -> Result<web::Json<Option<Receipt>>, ApiError> {
    let body = body.into_inner();

    let recorded = state
        .receipt
        .execute(&caller.0, body.notification_id, body.kind, body.device)
        .await?;

    // Receipts off is not an error (§7.3): `null` keeps one status and one response type.
    Ok(web::Json(recorded))
}

/// Who has read a notification — what unfolds under "seen by 3/5".
#[utoipa::path(
    get,
    path = "/notifications/{id}/receipts",
    tag = "notifications",
    params(("id" = String, Path, description = "Notification id")),
    responses(
        (status = 200,
         description = "The roll, or `null` when receipts are off on that channel",
         body = Option<Readership>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a member of its channel", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/notifications/{id}/receipts")]
pub(crate) async fn readership(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
) -> Result<web::Json<Option<Readership>>, ApiError> {
    let id: NotificationId = path.into_inner().parse()?;

    // `null`, exactly like `POST /receipts`.
    Ok(web::Json(state.readership.execute(&caller.0, id).await?))
}
