//! Publishing and reading notifications (design §8).

use actix_web::{HttpResponse, delete, get, post, put, web};
use notify_app::{InboxFilters, PublishRequest, Published, UpsertRequest};
use notify_domain::{
    ChannelId, ExternalId, Notification, NotificationId, Priority, Revision, RevisionDraft, State,
    Status,
};
use notify_ports::Page;
use serde::Deserialize;
use utoipa::{IntoParams, ToSchema};

use crate::extract::{Authenticated, csv, page, parse_enum};
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// `POST /notifications`: a revision, plus the channel it belongs to.
///
/// The channel is the only thing this shape adds to the upsert body — everything
/// else is the same revision draft, because there is exactly one ingestion shape
/// (design §8.3).
#[derive(Debug, Deserialize, ToSchema)]
pub struct PublishBody {
    /// Where it goes.
    pub channel: ChannelId,
    /// The revision itself, flattened into the same object.
    #[serde(flatten)]
    #[schema(inline)]
    pub draft: RevisionDraft,
}

/// Filters for a list of notifications.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct InboxParams {
    /// Comma-separated channel ids. Narrows what the caller may already read; it
    /// can never widen it.
    pub channels: Option<String>,
    /// Comma-separated lifecycle states: `open`, `working`, `closed`.
    pub state: Option<String>,
    /// Comma-separated statuses: `success`, `failure`, `warning`, `info`,
    /// `cancelled`.
    pub status: Option<String>,
    /// Comma-separated tags. All of them must be present.
    pub tag: Option<String>,
    /// Floor on priority: `min`, `low`, `default`, `high`, `urgent`.
    pub min_priority: Option<String>,
    /// Restrict to one client-side group.
    pub group_key: Option<String>,
    /// Opaque page cursor, from the previous page's `next`.
    pub cursor: Option<String>,
    /// Page size. Clamped server-side.
    pub limit: Option<u32>,
}

impl InboxParams {
    fn into_filters(self, channel: Option<ChannelId>) -> Result<InboxFilters, ApiError> {
        let mut channels = Vec::new();
        if let Some(one) = channel {
            channels.push(one);
        }
        for raw in csv(self.channels.as_deref()) {
            channels.push(ChannelId::parse(raw)?);
        }

        Ok(InboxFilters {
            channels,
            states: csv(self.state.as_deref())
                .into_iter()
                .map(|raw| parse_enum::<State>(raw, "state"))
                .collect::<Result<_, _>>()?,
            statuses: csv(self.status.as_deref())
                .into_iter()
                .map(|raw| parse_enum::<Status>(raw, "status"))
                .collect::<Result<_, _>>()?,
            tags: csv(self.tag.as_deref())
                .into_iter()
                .map(ToOwned::to_owned)
                .collect(),
            min_priority: self
                .min_priority
                .as_deref()
                .map(|raw| parse_enum::<Priority>(raw, "min_priority"))
                .transpose()?,
            group_key: self.group_key,
            page: page(self.cursor.as_deref(), self.limit)?,
        })
    }
}

/// A page request on its own.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PageParams {
    /// Opaque page cursor.
    pub cursor: Option<String>,
    /// Page size, clamped server-side.
    pub limit: Option<u32>,
}

/// Free-text search.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchParams {
    /// What to look for, in titles, bodies and tags.
    pub q: String,
    /// Opaque page cursor.
    pub cursor: Option<String>,
    /// Page size, clamped server-side.
    pub limit: Option<u32>,
}

/// Publish a notification.
#[utoipa::path(
    post,
    path = "/notifications",
    tag = "notifications",
    request_body = PublishBody,
    params(
        ("Idempotency-Key" = Option<String>, Header,
         description = "Replayed for 24 hours: the same key returns the same response \
                        rather than publishing twice."),
    ),
    responses(
        (status = 201, description = "Published", body = Published),
        (status = 200, description = "Replay of an earlier identical request", body = Published),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "No `publish` scope in that channel", body = Problem),
        (status = 422, description = "The payload does not validate", body = Problem),
        (status = 429, description = "Over the publish quota", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/notifications")]
pub(crate) async fn publish(
    state: web::Data<ApiState>,
    caller: Authenticated,
    request: actix_web::HttpRequest,
    body: web::Json<PublishBody>,
) -> Result<HttpResponse, ApiError> {
    let body = body.into_inner();
    let idempotency_key = request
        .headers()
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);

    let published = state
        .publish
        .execute(
            &caller.0,
            PublishRequest {
                channel: body.channel,
                draft: body.draft,
                idempotency_key,
            },
        )
        .await?;

    // A replay is not a creation: answering 201 twice would tell a client it
    // published twice, which is the one thing the header exists to prevent.
    let status = if published.replayed {
        actix_web::http::StatusCode::OK
    } else {
        actix_web::http::StatusCode::CREATED
    };
    Ok(HttpResponse::build(status).json(published))
}

/// Create or revise a notification by the publisher's own identifier.
#[utoipa::path(
    put,
    path = "/channels/{channel}/notifications/{external_id}",
    tag = "notifications",
    request_body = RevisionDraft,
    params(
        ("channel" = String, Path, description = "Channel id"),
        ("external_id" = String, Path, description = "The publisher's identifier, e.g. a CI run id"),
    ),
    responses(
        (status = 200, description = "Created or revised", body = Published),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "No `publish` scope in that channel", body = Problem),
        (status = 422, description = "The payload does not validate", body = Problem),
        (status = 429, description = "Over the publish quota", body = Problem),
    ),
    security(("bearer" = []))
)]
#[put("/channels/{channel}/notifications/{external_id}")]
pub(crate) async fn upsert(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<(String, String)>,
    body: web::Json<RevisionDraft>,
) -> Result<web::Json<Published>, ApiError> {
    let (channel, external_id) = path.into_inner();

    let published = state
        .upsert
        .execute(
            &caller.0,
            UpsertRequest {
                channel: ChannelId::parse(&channel)?,
                external_id: ExternalId::parse(&external_id)?,
                draft: body.into_inner(),
            },
        )
        .await?;

    // Always 200, never 201: whether this call created the notification or revised
    // it is an implementation detail of the retry the caller is making.
    Ok(web::Json(published))
}

/// The unified inbox: everything the caller may read.
#[utoipa::path(
    get,
    path = "/notifications",
    tag = "notifications",
    params(InboxParams),
    responses(
        (status = 200, description = "A page of notifications, newest first", body = Page<Notification>),
        (status = 401, description = "No usable credential", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/notifications")]
pub(crate) async fn inbox(
    state: web::Data<ApiState>,
    caller: Authenticated,
    params: web::Query<InboxParams>,
) -> Result<web::Json<Page<Notification>>, ApiError> {
    let filters = params.into_inner().into_filters(None)?;
    Ok(web::Json(state.inbox.execute(&caller.0, filters).await?))
}

/// One channel's inbox.
#[utoipa::path(
    get,
    path = "/channels/{channel}/notifications",
    tag = "notifications",
    params(("channel" = String, Path, description = "Channel id"), InboxParams),
    responses(
        (status = 200, description = "A page of notifications, newest first", body = Page<Notification>),
        (status = 401, description = "No usable credential", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/channels/{channel}/notifications")]
pub(crate) async fn channel_inbox(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
    params: web::Query<InboxParams>,
) -> Result<web::Json<Page<Notification>>, ApiError> {
    let channel = ChannelId::parse(&path.into_inner())?;
    let filters = params.into_inner().into_filters(Some(channel))?;
    Ok(web::Json(state.inbox.execute(&caller.0, filters).await?))
}

/// One notification.
#[utoipa::path(
    get,
    path = "/notifications/{id}",
    tag = "notifications",
    params(("id" = String, Path, description = "Notification id")),
    responses(
        (status = 200, description = "The notification as it now stands", body = Notification),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a member of its channel", body = Problem),
        (status = 409, description = "Deleted or expired (`notification.gone`)", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/notifications/{id}")]
pub(crate) async fn get_one(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
) -> Result<web::Json<Notification>, ApiError> {
    let id: NotificationId = path.into_inner().parse()?;
    Ok(web::Json(state.notification.execute(&caller.0, id).await?))
}

/// The revision history.
#[utoipa::path(
    get,
    path = "/notifications/{id}/revisions",
    tag = "notifications",
    params(("id" = String, Path, description = "Notification id"), PageParams),
    responses(
        (status = 200, description = "Revisions, oldest first", body = Page<Revision>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a member of its channel", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/notifications/{id}/revisions")]
pub(crate) async fn revisions(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
    params: web::Query<PageParams>,
) -> Result<web::Json<Page<Revision>>, ApiError> {
    let id: NotificationId = path.into_inner().parse()?;
    let params = params.into_inner();
    let page = page(params.cursor.as_deref(), params.limit)?;

    Ok(web::Json(
        state.revisions.execute(&caller.0, id, page).await?,
    ))
}

/// Delete a notification and broadcast its tombstone.
#[utoipa::path(
    delete,
    path = "/notifications/{id}",
    tag = "notifications",
    params(("id" = String, Path, description = "Notification id")),
    responses(
        (status = 204, description = "Deleted, or already was"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "No `publish` scope in its channel", body = Problem),
    ),
    security(("bearer" = []))
)]
#[delete("/notifications/{id}")]
pub(crate) async fn delete_one(
    state: web::Data<ApiState>,
    caller: Authenticated,
    path: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let id: NotificationId = path.into_inner().parse()?;
    state.delete_notification.execute(&caller.0, id).await?;
    Ok(HttpResponse::NoContent().finish())
}

/// Search within the caller's channels.
#[utoipa::path(
    get,
    path = "/search",
    tag = "notifications",
    params(SearchParams),
    responses(
        (status = 200, description = "Matching notifications", body = Page<Notification>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 429, description = "Over the search quota", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/search")]
pub(crate) async fn search(
    state: web::Data<ApiState>,
    caller: Authenticated,
    params: web::Query<SearchParams>,
) -> Result<web::Json<Page<Notification>>, ApiError> {
    let params = params.into_inner();
    let page = page(params.cursor.as_deref(), params.limit)?;

    Ok(web::Json(
        state.search.execute(&caller.0, &params.q, page).await?,
    ))
}
