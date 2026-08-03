//! The live transports: SSE and WebSocket (design §8.1, ADR-0001).
//!
//! Both carry the same events off the same cursor, and both are thin: a poke says
//! "something landed", and the connection reads the authoritative rows from the
//! event log through [`notify_app::SyncBacklog`] — the same use case `GET /sync`
//! calls. That is what keeps authorisation and visibility in one place rather than
//! re-derived per transport, and it is why "the events you missed" and "the events
//! arriving now" cannot disagree.

use std::time::Duration;

use actix_web::{HttpRequest, HttpResponse, get, web};
use actix_web_lab::sse;
use futures::StreamExt as _;
use notify_app::{Caller, SyncBacklog, SyncRequest};
use notify_domain::{ChannelId, Cursor, DeviceId};
use notify_ports::{Event, PageRequest};
use serde::Deserialize;
use utoipa::IntoParams;

use crate::extract::{Authenticated, csv};
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// Proxy-friendly, and short enough that a dead connection is noticed within a
/// reconnect rather than within a TCP timeout (design §8.1).
const KEEP_ALIVE: Duration = Duration::from_secs(25);

/// How many events a slow client may fall behind before it is dropped.
///
/// Dropping is the right answer: the client reconnects with `Last-Event-ID` and
/// replays exactly what it missed, whereas an unbounded buffer turns one stalled
/// browser tab into a memory leak on the replica.
const BACKPRESSURE: u32 = 64;

/// Where to resume, and what to narrow to.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StreamParams {
    /// Comma-separated channel ids; absent means every channel the caller may read.
    pub channels: Option<String>,
    /// Resume after this cursor. `Last-Event-ID` wins over it when both are sent,
    /// because that one is the browser reconnecting rather than the page asking.
    pub cursor: Option<String>,
    /// Which device is connected, so its cursor advances as it reads.
    pub device: Option<String>,
}

/// What a connection needs to keep pulling.
struct Connection {
    sync: SyncBacklog,
    caller: Caller,
    channels: Vec<ChannelId>,
    device: Option<DeviceId>,
    cursor: Cursor,
}

impl Connection {
    async fn open(
        state: &ApiState,
        caller: Caller,
        params: &StreamParams,
        last_event_id: Option<&str>,
    ) -> Result<Self, ApiError> {
        let channels = csv(params.channels.as_deref())
            .into_iter()
            .map(ChannelId::parse)
            .collect::<Result<Vec<_>, _>>()?;
        let device = params
            .device
            .as_deref()
            .map(str::parse::<DeviceId>)
            .transpose()?;

        // `Last-Event-ID` beats `?cursor=`: the header is the browser reconnecting
        // on its own, and it knows better than the URL the page was opened with.
        let resume = last_event_id
            .or(params.cursor.as_deref())
            .map(str::parse::<Cursor>)
            .transpose()?;

        let cursor = state.stream_start.execute(resume, device).await?;

        Ok(Self {
            sync: state.sync.clone(),
            caller,
            channels,
            device,
            cursor,
        })
    }

    /// Drain everything after the current cursor, advancing it as it goes.
    async fn drain(&mut self) -> Result<Vec<Event>, ApiError> {
        let mut drained = Vec::new();

        // A poke says "at least one thing landed", never how much: a replica that
        // was briefly behind has to catch up fully rather than one page at a time.
        loop {
            let page = self
                .sync
                .execute(
                    &self.caller,
                    SyncRequest {
                        cursor: Some(self.cursor),
                        channels: self.channels.clone(),
                        page: PageRequest::of_size(BACKPRESSURE),
                        device: self.device,
                    },
                )
                .await?;

            if let Some(last) = page.events.last() {
                self.cursor = last.cursor;
            }
            drained.extend(page.events);

            if page.next.is_none() {
                return Ok(drained);
            }
        }
    }
}

/// Render one event the way the SSE contract asks (design §8.1).
///
/// The `data:` is the whole event, not just its payload. §8.1 shows `channel_id`
/// inside the revision frame for a reason: a client that has never seen a
/// notification cannot file it anywhere without knowing its channel, and the
/// alternative is one `GET /notifications/{id}` per arrival — exactly the
/// follow-up [`notify_ports::RevisionEvent`] is denormalised to avoid.
///
/// It also makes the two live transports carry byte-identical JSON, which is what
/// §8.1 promises when it says the WebSocket transports the same events as frames.
fn as_sse(event: &Event) -> Result<sse::Data, serde_json::Error> {
    Ok(sse::Data::new_json(event)?
        // The `id:` is the cursor, which is what `Last-Event-ID` replays.
        .id(event.cursor.to_string())
        .event(event.kind().as_str()))
}

/// The live stream.
#[utoipa::path(
    get,
    path = "/stream",
    tag = "sync",
    params(
        StreamParams,
        ("Last-Event-ID" = Option<String>, Header,
         description = "The cursor of the last event received. Sent automatically by \
                        browsers on reconnect; replays exactly what was missed."),
    ),
    responses(
        (status = 200, description = "An event stream", content_type = "text/event-stream"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 403, description = "Not a device owner — service accounts read the inbox", body = Problem),
        (status = 422, description = "That cursor is not one of ours", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/stream")]
pub(crate) async fn stream(
    state: web::Data<ApiState>,
    caller: Authenticated,
    request: HttpRequest,
    params: web::Query<StreamParams>,
) -> Result<impl actix_web::Responder, ApiError> {
    let params = params.into_inner();
    let last_event_id = request
        .headers()
        .get("Last-Event-ID")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);

    let mut connection =
        Connection::open(&state, caller.0, &params, last_event_id.as_deref()).await?;
    let mut pokes = state.bus.subscribe().await?;

    let (sender, receiver) = tokio::sync::mpsc::channel(BACKPRESSURE as usize);
    let stream = sse::Sse::from_infallible_receiver(receiver).with_keep_alive(KEEP_ALIVE);

    // Everything the caller missed goes out before the first poke, so a reconnect
    // is seamless rather than "you get what happens from now on".
    for event in connection.drain().await? {
        let data = as_sse(&event).map_err(port_error)?;
        if sender.send(data.into()).await.is_err() {
            return Ok(stream);
        }
    }

    actix_web::rt::spawn(async move {
        while pokes.next().await.is_some() {
            let events = match connection.drain().await {
                Ok(events) => events,
                Err(error) => {
                    tracing::warn!(%error, "dropping a stream that cannot read the log");
                    break;
                }
            };

            for event in events {
                let Ok(data) = as_sse(&event) else {
                    tracing::error!(cursor = %event.cursor, "an event that will not serialise");
                    continue;
                };
                // A send failure is the client having gone away, which is the
                // ordinary end of a stream rather than something to report.
                if sender.send(data.into()).await.is_err() {
                    return;
                }
            }
        }
    });

    Ok(stream)
}

/// The same events, as WebSocket frames.
#[utoipa::path(
    get,
    path = "/ws",
    tag = "sync",
    params(StreamParams),
    responses(
        (status = 101, description = "Switching protocols; frames are the SSE payloads as JSON"),
        (status = 401, description = "No usable credential", body = Problem),
        (status = 422, description = "That cursor is not one of ours", body = Problem),
    ),
    security(("bearer" = []))
)]
#[get("/ws")]
pub(crate) async fn websocket(
    state: web::Data<ApiState>,
    caller: Authenticated,
    request: HttpRequest,
    body: web::Payload,
    params: web::Query<StreamParams>,
) -> Result<HttpResponse, ApiError> {
    let params = params.into_inner();
    let mut connection = Connection::open(&state, caller.0, &params, None).await?;
    let mut pokes = state.bus.subscribe().await?;

    let (response, mut session, _incoming) = match actix_ws::handle(&request, body) {
        Ok(parts) => parts,
        Err(error) => {
            // A failed handshake is the client not sending an upgrade, which is a
            // request problem rather than ours.
            tracing::debug!(%error, "websocket handshake refused");
            return Err(notify_domain::DomainError::Malformed {
                field: "upgrade",
                reason: "not a WebSocket handshake",
            }
            .into());
        }
    };

    actix_web::rt::spawn(async move {
        // Same order as SSE: backlog first, then live.
        loop {
            let events = match connection.drain().await {
                Ok(events) => events,
                Err(error) => {
                    tracing::warn!(%error, "dropping a socket that cannot read the log");
                    break;
                }
            };

            for event in events {
                let Ok(frame) = serde_json::to_string(&event) else {
                    tracing::error!(cursor = %event.cursor, "an event that will not serialise");
                    continue;
                };
                if session.text(frame).await.is_err() {
                    return;
                }
            }

            if pokes.next().await.is_none() {
                break;
            }
        }

        let _ = session.close(None).await;
    });

    Ok(response)
}

/// An event we cannot serialise is our bug, not the caller's.
fn port_error(error: serde_json::Error) -> ApiError {
    notify_ports::PortError::corrupt("event.payload", error).into()
}
