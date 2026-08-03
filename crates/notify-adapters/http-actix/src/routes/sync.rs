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
    /// Which device is catching up, so its cursor is advanced. Absent for a one-off
    /// caller — `curl /sync` from a shell has no cursor to keep.
    pub device: Option<String>,
    /// Page size, clamped server-side.
    pub limit: Option<u32>,
}

/// Replay everything the caller missed.
///
/// The same cursor the live stream stamps on each event, so a client that was
/// offline for a week and one whose connection blipped take this identical path
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
        // The page cursor and the sync cursor are the same token, so only the size
        // half of the page request is meaningful here.
        page: page(None, params.limit)?,
        device,
    };

    Ok(web::Json(state.sync.execute(&caller.0, request).await?))
}
