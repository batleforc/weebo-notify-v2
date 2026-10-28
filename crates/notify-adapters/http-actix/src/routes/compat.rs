//! Gotify, ntfy and Alertmanager ingestion, so existing senders only change URL and token,
//! and any JSON webhook through an operator-declared transform (RFC 0003).
//! Every route feeds the same publish/upsert use cases as `POST /notifications`.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use actix_web::http::header;
use actix_web::{
    FromRequest, HttpMessage as _, HttpRequest, HttpResponse, dev::Payload, http::StatusCode, mime,
    post, put, web,
};
use base64::Engine as _;
use notify_app::{Caller, Grant, IngestError, PublishRequest, Published, UpsertRequest, Upserted};
use notify_domain::limits::MAX_TAG_CHARS;
use notify_domain::{
    Action, ActionDraft, Actions, ChannelId, ContentDraft, DomainError, ExternalId, NotificationId,
    Priority, RevisionDraft, State, Status,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::extract::{Authenticated, authenticate, csv};
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// actix's `JsonConfig` default, which is what bounds `POST /notifications`.
const BODY_LIMIT: usize = 2 * 1024 * 1024;

/// A JSON webhook's ceiling: evaluation cost grows with the body (RFC 0003 §7.5).
const JSON_BODY_LIMIT: usize = 256 * 1024;

/// A caller authenticated the way Gotify/ntfy clients send keys: `Bearer`, `Basic`
/// (password = key), `X-Gotify-Key`, or `?token=`. Only these routes accept the latter.
#[derive(Debug)]
pub(crate) struct CompatAuth(Caller);

impl FromRequest for CompatAuth {
    type Error = ApiError;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let auth = authenticate(req, presented(req));
        Box::pin(async move { auth.await.map(|Authenticated(caller)| Self(caller)) })
    }
}

/// Normalise every accepted form into a `Bearer` header value.
fn presented(req: &HttpRequest) -> Option<String> {
    let headers = req.headers();
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if let Some(basic) = authorization.and_then(|raw| {
        raw.strip_prefix("Basic ")
            .or_else(|| raw.strip_prefix("basic "))
    }) {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(basic.trim())
            .ok()?;
        let (_, key) = std::str::from_utf8(&decoded).ok()?.split_once(':')?;
        return Some(format!("Bearer {key}"));
    }
    if let Some(raw) = authorization {
        return Some(raw.to_owned());
    }
    let key = headers
        .get("X-Gotify-Key")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
        .or_else(|| CompatQuery::of(req).token)?;
    Some(format!("Bearer {key}"))
}

/// Query parameters every compat route understands.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CompatQuery {
    /// The API key, for clients that cannot set a header. Prefer a header: URLs get logged.
    pub token: Option<String>,
    /// Target channel, required when the key is scoped to more than one.
    pub channel: Option<String>,
}

impl CompatQuery {
    fn of(req: &HttpRequest) -> Self {
        web::Query::<Self>::from_query(req.query_string())
            .map(web::Query::into_inner)
            .unwrap_or_default()
    }
}

/// A Gotify `POST /message` body, as JSON or a form.
#[derive(Debug, Deserialize, ToSchema)]
pub struct GotifyBody {
    /// Title; defaults to the channel id.
    pub title: Option<String>,
    /// The message text.
    pub message: String,
    /// Gotify priority: 0-3 low, 4-7 default, 8 high, 9 and above urgent.
    pub priority: Option<i64>,
    /// Gotify extras; `client::notification.click.url` becomes a `view` action.
    #[schema(value_type = Option<Object>)]
    pub extras: Option<serde_json::Value>,
}

/// What Gotify answers, so clients that parse the response keep working.
#[derive(Debug, Serialize, ToSchema)]
pub struct GotifyMessage {
    /// The event-log position of this publish.
    pub id: u64,
    /// Always 0: there are no Gotify applications here.
    pub appid: u8,
    /// The message text.
    pub message: String,
    /// The title as published.
    pub title: String,
    /// The priority as sent.
    pub priority: i64,
    /// When the server received it (RFC 3339).
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String)]
    pub date: time::OffsetDateTime,
}

/// An ntfy JSON publish, sent to the ntfy root.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NtfyBody {
    /// The channel to publish into.
    pub topic: String,
    /// The message text.
    pub message: Option<String>,
    /// Title; defaults to the topic.
    pub title: Option<String>,
    /// ntfy priority, 1 (min) to 5 (urgent).
    pub priority: Option<u8>,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// A link, published as a `view` action.
    pub click: Option<String>,
}

/// What ntfy answers to a publish.
#[derive(Debug, Serialize, ToSchema)]
pub struct NtfyMessage {
    /// The notification id.
    pub id: String,
    /// Unix seconds when the server received it.
    pub time: i64,
    /// Always `message`.
    pub event: &'static str,
    /// The channel it was published into.
    pub topic: String,
    /// The message text.
    pub message: String,
}

/// The Alertmanager webhook payload; only the fields used here are described.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AlertmanagerBody {
    /// One notification is upserted per alert.
    pub alerts: Vec<Alert>,
}

/// One alert of an Alertmanager webhook.
#[derive(Debug, Deserialize, ToSchema)]
pub struct Alert {
    /// `firing` or `resolved`.
    pub status: String,
    /// `alertname` and `severity` are read; `severity`, `job` and `namespace` become tags.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// `summary` joins the title, `description` is the body.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    /// Upsert key, so a resolution revises the notification its firing created.
    #[serde(default)]
    pub fingerprint: String,
}

/// Publish a message the way a Gotify client does.
#[utoipa::path(
    post,
    path = "/compat/gotify/message",
    tag = "compat",
    params(CompatQuery),
    request_body(content = GotifyBody, content_type = "application/json"),
    responses(
        (status = 200, description = "Published", body = GotifyMessage),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "No `publish` scope in that channel", body = Problem),
        (status = 422, description = "Invalid payload, or the key spans several channels and no `?channel=` was given", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/compat/gotify/message")]
pub(crate) async fn gotify(
    state: web::Data<ApiState>,
    caller: CompatAuth,
    request: HttpRequest,
    payload: web::Payload,
) -> Result<web::Json<GotifyMessage>, ApiError> {
    let body: GotifyBody = decode(&request, &read(payload, BODY_LIMIT).await?)?;
    let channel = match CompatQuery::of(&request).channel {
        Some(raw) => ChannelId::parse(&raw)?,
        None => sole_channel(&caller.0).ok_or(DomainError::Missing { field: "channel" })?,
    };
    let click = body
        .extras
        .as_ref()
        .and_then(|extras| extras.pointer("/client::notification/click/url")?.as_str());
    let priority = body.priority.map(|p| match p {
        ..=3 => Priority::Low,
        4..=7 => Priority::Default,
        8 => Priority::High,
        _ => Priority::Urgent,
    });
    let title = body.title.clone().unwrap_or_else(|| channel.to_string());
    let draft = draft(
        title,
        Some(body.message.clone()),
        priority,
        Vec::new(),
        click,
    )?;
    let published = publish(&state, &caller.0, channel, draft).await?;

    Ok(web::Json(GotifyMessage {
        id: published.cursor.sequence(),
        appid: 0,
        message: body.message,
        title: published.revision.content.title,
        priority: body.priority.unwrap_or(0),
        date: published.revision.received_at,
    }))
}

/// Publish to an ntfy topic: the body is the message, headers carry the rest.
#[utoipa::path(
    post,
    path = "/compat/ntfy/{channel}",
    tag = "compat",
    params(
        ("channel" = String, Path, description = "Channel id (the ntfy topic)"),
        ("Title" = Option<String>, Header, description = "Also `X-Title` or `t`"),
        ("Priority" = Option<String>, Header, description = "1-5 or min/low/default/high/urgent (max); also `X-Priority`, `p`"),
        ("Tags" = Option<String>, Header, description = "Comma-separated; also `X-Tags`, `ta`"),
        ("Click" = Option<String>, Header, description = "A link, published as a `view` action; also `X-Click`"),
        ("token" = Option<String>, Query, description = "The API key, for clients that cannot set a header. Prefer a header: URLs get logged."),
    ),
    request_body(content = String, content_type = "text/plain"),
    responses(
        (status = 200, description = "Published", body = NtfyMessage),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "No `publish` scope in that channel", body = Problem),
        (status = 422, description = "The payload does not validate", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/compat/ntfy/{channel}")]
pub(crate) async fn ntfy(
    state: web::Data<ApiState>,
    caller: CompatAuth,
    request: HttpRequest,
    path: web::Path<String>,
    payload: web::Payload,
) -> Result<web::Json<NtfyMessage>, ApiError> {
    ntfy_text(state, caller, request, path, payload).await
}

/// Same as `POST`: ntfy clients use either.
#[utoipa::path(
    put,
    path = "/compat/ntfy/{channel}",
    tag = "compat",
    params(("channel" = String, Path, description = "Channel id (the ntfy topic)"), ("token" = Option<String>, Query, description = "The API key, for clients that cannot set a header. Prefer a header: URLs get logged.")),
    request_body(content = String, content_type = "text/plain"),
    responses((status = 200, description = "Published", body = NtfyMessage)),
    security(("bearer" = []))
)]
#[put("/compat/ntfy/{channel}")]
pub(crate) async fn ntfy_put(
    state: web::Data<ApiState>,
    caller: CompatAuth,
    request: HttpRequest,
    path: web::Path<String>,
    payload: web::Payload,
) -> Result<web::Json<NtfyMessage>, ApiError> {
    ntfy_text(state, caller, request, path, payload).await
}

async fn ntfy_text(
    state: web::Data<ApiState>,
    caller: CompatAuth,
    request: HttpRequest,
    path: web::Path<String>,
    payload: web::Payload,
) -> Result<web::Json<NtfyMessage>, ApiError> {
    let body = read(payload, BODY_LIMIT).await?;
    let message = String::from_utf8_lossy(&body).into_owned();
    let header = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| request.headers().get(*name)?.to_str().ok())
    };
    let priority = header(&["Priority", "X-Priority", "prio", "p"])
        .map(ntfy_priority)
        .transpose()?;
    let tags = csv(header(&["Tags", "X-Tags", "tag", "ta"]))
        .into_iter()
        .map(ToOwned::to_owned)
        .collect();
    let message = NtfyBody {
        topic: path.into_inner(),
        message: Some(message),
        title: header(&["Title", "X-Title", "t"]).map(ToOwned::to_owned),
        priority: None,
        tags,
        click: header(&["Click", "X-Click"]).map(ToOwned::to_owned),
    };
    ntfy_publish(&state, &caller.0, message, priority).await
}

/// Publish an ntfy JSON message; the channel is its `topic`.
#[utoipa::path(
    post,
    path = "/compat/ntfy",
    tag = "compat",
    params(CompatQuery),
    request_body = NtfyBody,
    responses(
        (status = 200, description = "Published", body = NtfyMessage),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "No `publish` scope in that channel", body = Problem),
        (status = 422, description = "The payload does not validate", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/compat/ntfy")]
pub(crate) async fn ntfy_json(
    state: web::Data<ApiState>,
    caller: CompatAuth,
    request: HttpRequest,
    payload: web::Payload,
) -> Result<web::Json<NtfyMessage>, ApiError> {
    let body: NtfyBody = decode(&request, &read(payload, BODY_LIMIT).await?)?;
    let priority = body
        .priority
        .map(|p| ntfy_priority(&p.to_string()))
        .transpose()?;
    ntfy_publish(&state, &caller.0, body, priority).await
}

/// Receive an Alertmanager webhook: one notification per alert, upserted by fingerprint.
#[utoipa::path(
    post,
    path = "/compat/alertmanager/{channel}",
    tag = "compat",
    params(("channel" = String, Path, description = "Channel id"), ("token" = Option<String>, Query, description = "The API key, for clients that cannot set a header. Prefer a header: URLs get logged.")),
    request_body = AlertmanagerBody,
    responses(
        (status = 200, description = "Each alert's notification as it now stands", body = Vec<Published>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "No `publish` scope in that channel", body = Problem),
        (status = 422, description = "The payload does not validate", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/compat/alertmanager/{channel}")]
pub(crate) async fn alertmanager(
    state: web::Data<ApiState>,
    caller: CompatAuth,
    request: HttpRequest,
    path: web::Path<String>,
    payload: web::Payload,
) -> Result<web::Json<Vec<Published>>, ApiError> {
    let body: AlertmanagerBody = decode(&request, &read(payload, BODY_LIMIT).await?)?;
    let channel = ChannelId::parse(&path.into_inner())?;

    // Sequential and idempotent per fingerprint, so Alertmanager's retry of a failed
    // batch only re-applies what already landed.
    let mut published = Vec::with_capacity(body.alerts.len());
    for alert in body.alerts {
        let label = |key: &str| alert.labels.get(key).map(String::as_str);
        let severity = label("severity");
        let (lifecycle, status) = if alert.status == "resolved" {
            (State::Closed, Status::Success)
        } else {
            let status = match severity {
                Some("critical") => Status::Failure,
                Some("warning") => Status::Warning,
                _ => Status::Info,
            };
            (State::Open, status)
        };
        let priority = match severity {
            Some("critical") => Some(Priority::Urgent),
            Some("warning") => Some(Priority::High),
            _ => None,
        };
        let title: Vec<&str> = [
            label("alertname"),
            alert.annotations.get("summary").map(String::as_str),
        ]
        .into_iter()
        .flatten()
        .collect();
        let title = if title.is_empty() {
            "alert".to_owned()
        } else {
            title.join(": ")
        };
        let tags = ["severity", "job", "namespace"]
            .into_iter()
            .filter_map(label)
            .filter(|tag| tag.chars().count() <= MAX_TAG_CHARS)
            .map(ToOwned::to_owned)
            .collect();
        let mut draft = draft(
            title,
            alert.annotations.get("description").cloned(),
            priority,
            tags,
            None,
        )?;
        draft.state = Some(lifecycle);
        draft.status = Some(status);

        let one = state
            .upsert
            .execute(
                &caller.0,
                UpsertRequest {
                    channel: channel.clone(),
                    external_id: ExternalId::parse(&alert.fingerprint)?,
                    draft,
                },
            )
            .await?;
        state.metrics.published(&one.notification.channel);
        published.push(one);
    }
    Ok(web::Json(published))
}

/// One element of a JSON webhook: its notification as written, or only its id
/// when it was already there (RFC 0003 decision 11): a skip carries no content.
#[derive(Debug, Serialize, ToSchema)]
#[serde(untagged)]
pub enum JsonIngested {
    Written(Box<Published>),
    Skipped {
        notification_id: NotificationId,
        /// Always `true`.
        skipped: bool,
    },
}

/// Receive any JSON webhook, mapped by the operator-declared transform `{transform}`.
#[utoipa::path(
    post,
    path = "/compat/json/{transform}/{channel}",
    tag = "compat",
    params(
        ("transform" = String, Path, description = "A `[[transforms]]` id"),
        ("channel" = String, Path, description = "Channel id; must be in the transform's `channels`"),
        ("token" = Option<String>, Query, description = "The API key, for clients that cannot set a header. Prefer a header: URLs get logged."),
    ),
    request_body(content = Object, content_type = "application/json"),
    responses(
        (status = 200, description = "One entry per element, in order", body = Vec<JsonIngested>),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "No `publish` scope in that channel", body = Problem),
        (status = 404, description = "No such transform, or it does not serve that channel", body = Problem),
        (status = 413, description = "Over 256 KiB", body = Problem),
        (status = 415, description = "A form or multipart body", body = Problem),
        (status = 422, description = "Not JSON, `split` is not a list, too many elements, or every element refused", body = Problem),
        (status = 429, description = "Over the publish quota; elements before it stay published", body = Problem),
        (status = 503, description = "Every evaluation slot is busy, or the evaluation took too long", body = Problem),
    ),
    security(("bearer" = []))
)]
#[post("/compat/json/{transform}/{channel}")]
pub(crate) async fn json(
    state: web::Data<ApiState>,
    caller: CompatAuth,
    request: HttpRequest,
    path: web::Path<(String, String)>,
    payload: web::Payload,
) -> Result<HttpResponse, ApiError> {
    let (transform, channel) = path.into_inner();
    let body = match read(payload, JSON_BODY_LIMIT).await {
        Ok(body) => body,
        Err(error) => {
            if matches!(error, DomainError::TooLarge { .. }) {
                state.ingest_json.refused_early(&transform, "too_large");
            }
            return Err(error.into());
        }
    };
    let mime = request.mime_type().ok().flatten();
    if matches!(
        mime.as_ref().map(mime::Mime::essence_str),
        Some("application/x-www-form-urlencoded" | "multipart/form-data")
    ) {
        state.ingest_json.refused_early(&transform, "form");
        return Ok(problem(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "a JSON webhook takes a JSON body, not a form",
            "body.not_json",
        ));
    }
    let channel = ChannelId::parse(&channel)?;

    match state
        .ingest_json
        .execute(&caller.0, &transform, channel, body.to_vec())
        .await
    {
        Ok(elements) => {
            let answer: Vec<JsonIngested> = elements
                .into_iter()
                .map(|element| match element {
                    Upserted::Written(published) => {
                        state.metrics.published(&published.notification.channel);
                        JsonIngested::Written(published)
                    }
                    Upserted::Skipped(notification_id) => JsonIngested::Skipped {
                        notification_id,
                        skipped: true,
                    },
                })
                .collect();
            Ok(HttpResponse::Ok().json(answer))
        }
        Err(IngestError::Refused(failure)) => Err(failure.into()),
        // One body for an unknown id and an unlisted channel, byte for byte (T9).
        Err(IngestError::NotFound) => Ok(problem(
            StatusCode::NOT_FOUND,
            "no such transform for this channel",
            "transform.not_found",
        )),
        Err(IngestError::Busy) => {
            let mut busy = problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "every evaluation slot is busy; retry shortly",
                "transform.busy",
            );
            busy.headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
            Ok(busy)
        }
    }
}

/// A refusal the domain has no kind for: `404`, `415`, `503`.
fn problem(status: StatusCode, title: &str, code: &'static str) -> HttpResponse {
    HttpResponse::build(status)
        .insert_header((header::CONTENT_TYPE, "application/problem+json"))
        .json(Problem {
            kind: "about:blank",
            title: title.to_owned(),
            status: status.as_u16(),
            code: Some(code),
            retry_after_seconds: None,
        })
}

async fn ntfy_publish(
    state: &ApiState,
    caller: &Caller,
    body: NtfyBody,
    priority: Option<Priority>,
) -> Result<web::Json<NtfyMessage>, ApiError> {
    let channel = ChannelId::parse(&body.topic)?;
    let message = body.message.unwrap_or_default();
    let title = body.title.unwrap_or_else(|| channel.to_string());
    let text = (!message.is_empty()).then(|| message.clone());
    let draft = draft(title, text, priority, body.tags, body.click.as_deref())?;
    let published = publish(state, caller, channel, draft).await?;

    Ok(web::Json(NtfyMessage {
        id: published.notification.id.to_string(),
        time: published.revision.received_at.unix_timestamp(),
        event: "message",
        topic: published.notification.channel.to_string(),
        message,
    }))
}

async fn publish(
    state: &ApiState,
    caller: &Caller,
    channel: ChannelId,
    draft: RevisionDraft,
) -> Result<Published, ApiError> {
    let request = PublishRequest {
        channel,
        draft,
        idempotency_key: None,
    };
    let published = state.publish.execute(caller, request).await?;
    state.metrics.published(&published.notification.channel);
    Ok(published)
}

fn draft(
    title: String,
    body: Option<String>,
    priority: Option<Priority>,
    tags: Vec<String>,
    click: Option<&str>,
) -> Result<RevisionDraft, DomainError> {
    let actions = click
        .map(|url| {
            Action::try_from(ActionDraft::View {
                label: "Open".to_owned(),
                url: url.to_owned(),
            })
        })
        .transpose()?;
    let mut draft = RevisionDraft::simple(ContentDraft {
        title,
        body,
        tags,
        actions: Actions::new(actions.into_iter().collect())?,
        ..ContentDraft::default()
    });
    draft.priority = priority;
    Ok(draft)
}

/// The key's only channel, when it has exactly one.
fn sole_channel(caller: &Caller) -> Option<ChannelId> {
    match &caller.grant {
        Grant::ApiKey { channels, .. } if channels.len() == 1 => channels.first().cloned(),
        Grant::Federated { grants } if grants.len() == 1 => {
            grants.first().map(|(id, _)| id.clone())
        }
        _ => None,
    }
}

fn ntfy_priority(raw: &str) -> Result<Priority, DomainError> {
    Ok(match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "min" => Priority::Min,
        "2" | "low" => Priority::Low,
        "3" | "default" => Priority::Default,
        "4" | "high" => Priority::High,
        "5" | "urgent" | "max" => Priority::Urgent,
        _ => {
            return Err(DomainError::Malformed {
                field: "priority",
                reason: "not one of the documented values",
            });
        }
    })
}

async fn read(payload: web::Payload, limit: usize) -> Result<web::Bytes, DomainError> {
    match payload.to_bytes_limited(limit).await {
        Ok(Ok(body)) => Ok(body),
        Ok(Err(_)) => Err(DomainError::Malformed {
            field: "body",
            reason: "the upload did not complete",
        }),
        Err(_) => Err(DomainError::TooLarge {
            what: "body",
            max: limit,
            actual: limit.saturating_add(1),
        }),
    }
}

/// JSON, or a urlencoded/multipart form when the client says so (Gotify takes all three).
fn decode<T: DeserializeOwned>(request: &HttpRequest, body: &[u8]) -> Result<T, DomainError> {
    let malformed = DomainError::Malformed {
        field: "body",
        reason: "not a valid JSON or form body",
    };
    let mime = request.mime_type().ok().flatten();
    let text = std::str::from_utf8(body).ok();
    let form = match mime.as_ref().map(mime::Mime::essence_str) {
        Some("application/x-www-form-urlencoded") => text.map(ToOwned::to_owned),
        Some("multipart/form-data") => mime
            .as_ref()
            .and_then(|m| m.get_param(mime::BOUNDARY))
            .zip(text)
            .map(|(boundary, text)| multipart_fields(text, boundary.as_str())),
        _ => return serde_json::from_slice(body).map_err(|_| malformed),
    };
    form.and_then(|form| web::Query::<T>::from_query(&form).ok())
        .map(web::Query::into_inner)
        .ok_or(malformed)
}

/// The text fields of a multipart body, re-encoded as a urlencoded form.
fn multipart_fields(body: &str, boundary: &str) -> String {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    for part in body.split(&format!("--{boundary}")) {
        let Some((head, value)) = part.split_once("\r\n\r\n") else {
            continue;
        };
        if let Some(name) = head
            .split("name=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
        {
            form.append_pair(name, value.strip_suffix("\r\n").unwrap_or(value));
        }
    }
    form.finish()
}
