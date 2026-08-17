//! `/api/v1/admin/*` — the platform console (design §5.4, §8, §10.4bis).
//!
//! Everything here needs the platform role, and everything here refuses **in the
//! use case** rather than in this file. That is not belt and braces: a check living
//! only in a route table is a check the next route table can be written without,
//! and this is the one boundary where forgetting is not recoverable.
//!
//! # The DLQ answers about deliveries, never about their contents
//!
//! `GET /admin/dlq` names a channel, a sink and an error, and stops there. The
//! event that failed to deliver is in the queue row and is not served: an admin who
//! is not a member of the channel would otherwise read its notifications through
//! the failure of a webhook, which is the one door §5.4 closes.

use actix_web::{HttpResponse, get, post, web};
use notify_app::{ChannelCard, Reconciliation, TrustPolicyCard};
use notify_ports::{AuditEntry, AuditQuery, DeadLetter, Page};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::extract::Authenticated;
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// Filters for the audit log — the after-incident questions (design §13).
#[derive(Debug, Default, Deserialize, ToSchema)]
pub(crate) struct AuditFilters {
    /// Restrict to one action, e.g. `apikey.revoked`.
    pub(crate) action: Option<String>,
    /// Restrict to one channel.
    pub(crate) channel: Option<String>,
    /// Opaque page cursor.
    pub(crate) cursor: Option<String>,
    /// Page size, clamped server-side.
    pub(crate) limit: Option<u32>,
}

/// Browse the audit log.
#[utoipa::path(
    get,
    path = "/admin/audit",
    tag = "admin",
    // Declared here as well as on the handler, and that is not duplication: the
    // document is what the generated clients are built from (ADR-0009), so a filter
    // the handler accepts and the document omits is a filter no client can send.
    params(
        ("action" = Option<String>, Query, description = "One action, e.g. `apikey.revoked`"),
        ("channel" = Option<String>, Query, description = "One channel"),
        ("cursor" = Option<String>, Query, description = "Opaque page cursor"),
        ("limit" = Option<u32>, Query, description = "Page size, clamped server-side"),
    ),
    responses(
        (status = 200, description = "Audit entries, newest first", body = Page<AuditEntry>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a platform admin", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/admin/audit")]
pub(crate) async fn audit(
    state: web::Data<ApiState>,
    caller: Authenticated,
    filters: web::Query<AuditFilters>,
) -> Result<web::Json<Page<AuditEntry>>, ApiError> {
    let filters = filters.into_inner();
    let channel = filters.channel.as_deref().map(str::parse).transpose()?;

    Ok(web::Json(
        state
            .audit
            .execute(
                &caller.0,
                AuditQuery {
                    actor: None,
                    action: filters.action,
                    channel,
                    since: None,
                    until: None,
                    page: crate::extract::page(filters.cursor.as_deref(), filters.limit)?,
                },
            )
            .await?,
    ))
}

/// The channel inventory — record cards, never inboxes.
#[utoipa::path(
    get,
    path = "/admin/channels",
    tag = "admin",
    responses(
        (status = 200, description = "Every channel, as metadata", body = Vec<ChannelCard>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a platform admin", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/admin/channels")]
pub(crate) async fn inventory(
    state: web::Data<ApiState>,
    caller: Authenticated,
) -> Result<web::Json<Vec<ChannelCard>>, ApiError> {
    Ok(web::Json(state.inventory.execute(&caller.0).await?))
}

/// Join a channel as the calling admin — the audited way to content (design §5.4).
#[utoipa::path(
    post,
    path = "/admin/channels/{channel}/join",
    tag = "admin",
    params(("channel" = String, Path, description = "Channel id")),
    responses(
        (status = 204, description = "Joined, and recorded in the audit log"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a platform admin", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/admin/channels/{channel}/join")]
pub(crate) async fn self_join(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let id = path.into_inner().parse()?;
    state.self_join.execute(&caller.0, &id).await?;

    // `204` and not the channel: answering with its contents would be this route
    // quietly becoming the content read the whole boundary exists to prevent. The
    // admin is a member now and reads it the way every member does.
    Ok(HttpResponse::NoContent().finish())
}

/// The trust policies this instance was deployed with (design §5.3, §11).
#[utoipa::path(
    get,
    path = "/admin/trust-policies",
    tag = "admin",
    responses(
        (status = 200, description = "Configured policies, read-only", body = Vec<TrustPolicyCard>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a platform admin", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/admin/trust-policies")]
pub(crate) async fn trust_policies(
    state: web::Data<ApiState>,
    caller: Authenticated,
) -> Result<web::Json<Vec<TrustPolicyCard>>, ApiError> {
    // No `POST` beside it, and there will not be one: §11 owns these, and an edit
    // button would offer what the next reconciliation takes back.
    Ok(web::Json(state.trust_policies.execute(&caller.0)?))
}

/// Which channel's dead letters to show.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub(crate) struct DlqFilters {
    /// Restrict to one channel. Absent means every channel.
    pub(crate) channel: Option<String>,
}

/// The sink deliveries that were given up on (design §7.1bis, §10.4bis).
#[utoipa::path(
    get,
    path = "/admin/dlq",
    tag = "admin",
    params(("channel" = Option<String>, Query, description = "One channel")),
    responses(
        (status = 200, description = "Dead letters, newest first", body = Vec<DeadLetter>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a platform admin", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/admin/dlq")]
pub(crate) async fn dlq(
    state: web::Data<ApiState>,
    caller: Authenticated,
    filters: web::Query<DlqFilters>,
) -> Result<web::Json<Vec<DeadLetter>>, ApiError> {
    let channel = filters
        .into_inner()
        .channel
        .as_deref()
        .map(str::parse)
        .transpose()?;

    Ok(web::Json(
        state.dlq.execute(&caller.0, channel.as_ref()).await?,
    ))
}

/// Put one dead letter back in the queue (design §8, scenario T17).
#[utoipa::path(
    post,
    path = "/admin/dlq/{id}/requeue",
    tag = "admin",
    params(("id" = String, Path, description = "Delivery id, as `GET /admin/dlq` returns it")),
    responses(
        (status = 204, description = "Requeued, and recorded in the audit log"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a platform admin", body = Problem),
            (status = 409, description = "No dead letter with that id — already requeued, or purged", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/admin/dlq/{id}/requeue")]
pub(crate) async fn requeue_dead_letter(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let id = path.into_inner().parse()?;

    // A refusal and not a `204` for one that is not there: a console that reported
    // success for a requeue that requeued nothing would leave an operator waiting on
    // a delivery nobody scheduled. `409` rather than `404`, like every other object
    // this API says is gone.
    if state.requeue.execute(&caller.0, &id).await? {
        Ok(HttpResponse::NoContent().finish())
    } else {
        Err(notify_domain::DomainError::DeadLetterGone.into())
    }
}

/// What the last GitOps pass did on this replica (design §11).
#[utoipa::path(
    get,
    path = "/admin/reconciliation",
    tag = "admin",
    responses(
        (status = 200,
         description = "The last pass, or `null` if none has finished on this replica",
         body = Option<Reconciliation>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a platform admin", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/admin/reconciliation")]
pub(crate) async fn reconciliation(
    state: web::Data<ApiState>,
    caller: Authenticated,
) -> Result<web::Json<Option<Reconciliation>>, ApiError> {
    // This replica's, not the instance's: every replica reconciles (§11 gates only
    // the purge on leadership), so behind a load balancer two reads can differ by a
    // tick. The field says so rather than the answer pretending otherwise.
    Ok(web::Json(state.reconciliation.execute(&caller.0)?))
}
