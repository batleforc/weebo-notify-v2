//! What comes back from a notification: interactions, silences, receipts.

use actix_web::{get, post, web};
use notify_domain::{
    DeviceId, HumanDuration, Interaction, InteractionRecord, NotificationId, Receipt, ReceiptKind,
    Recorded, Silence, SilenceScope,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// What recording an interaction did.
///
/// A DTO rather than the domain's [`Recorded`], because the distinction the domain
/// draws — did this change anything worth broadcasting — is exactly what a client
/// needs to hear, and the enum's shape (a tuple variant, a bare id) is not a good
/// JSON contract.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecordedInteraction {
    /// `created`, `unchanged` or `replaced`.
    #[schema(example = "created")]
    pub outcome: &'static str,
    /// The interaction that now stands. Absent on `unchanged`, where the caller
    /// already has it.
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

    // Always 200, never 201: clicking an `ack` twice is one interaction, and a
    // client that retries must not be told it created a second one.
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

    // A channel with receipts off is not an error — the client posts them
    // unconditionally and the channel policy decides (design §7.3). `null` rather
    // than a 204: one status with a nullable body is one branch for the caller,
    // and it keeps the generated clients to a single response type.
    Ok(web::Json(recorded))
}
