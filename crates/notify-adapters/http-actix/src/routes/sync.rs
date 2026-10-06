//! Catching up: the offline backlog (design §8.1, ADR-0012).

use actix_web::{get, web};
use notify_app::SyncRequest;
use notify_domain::{ChannelId, Cursor, DeviceId};
use notify_ports::SyncPage;
use serde::Deserialize;
use utoipa::IntoParams;

use crate::extract::{Authenticated, csv, page};
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// Where to resume from, and what to narrow to.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SyncParams {
    /// Resume after this cursor. Absent replays everything still retained.
    pub cursor: Option<String>,
    /// Comma-separated channel ids; absent means every channel the caller may read.
    pub channels: Option<String>,
    /// Which device is catching up, so its cursor advances; absent for a one-off caller.
    pub device: Option<String>,
    /// Page size, clamped server-side.
    pub limit: Option<u32>,
}

/// Replay everything the caller missed, off the same cursor the live stream stamps
/// (ADR-0001, ADR-0012).
#[utoipa::path(
    get,
    path = "/sync",
    tag = "sync",
    params(SyncParams),
    responses(
        (status = 200,
         description = "Events since the cursor, plus what expired while away",
         body = SyncPage),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a device owner — service accounts read the inbox", body = Problem),
        (status = 422, description = "That cursor is not one of ours", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/sync")]
pub(crate) async fn sync(
    state: web::Data<ApiState>,
    caller: Authenticated,
    params: web::Query<SyncParams>,
) -> Result<web::Json<SyncPage>, ApiError> {
    let params = params.into_inner();

    let channels = csv(params.channels.as_deref())
        .into_iter()
        .map(ChannelId::parse)
        .collect::<Result<Vec<_>, _>>()?;
    let device = params
        .device
        .as_deref()
        .map(str::parse::<DeviceId>)
        .transpose()?;
    let cursor = params
        .cursor
        .as_deref()
        .map(str::parse::<Cursor>)
        .transpose()?;

    let request = SyncRequest {
        cursor,
        channels,
        // Page and sync cursor are the same token; only the size half matters here.
        page: page(None, params.limit)?,
        device,
    };

    let mut page = state.sync.execute(&caller.0, request).await?;

    // The same per-event DND flag the stream sets (§7.4, D22).
    for event in &mut page.events {
        if let notify_ports::EventPayload::Revision(revision) = &event.payload
            && page.deferred_by_dnd.contains(&revision.notification_id)
        {
            event.muted_by_dnd = true;
        }
    }

    // Reconnect lag (design §13); not in the stream, where every poke would count as one.
    state.metrics.sync_backlog(page.events.len());

    Ok(web::Json(page))
}
