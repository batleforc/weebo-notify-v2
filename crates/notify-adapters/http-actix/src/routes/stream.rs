//! The live transports, SSE and WebSocket (design §8.1, ADR-0001). A poke only wakes
//! the connection, which reads rows via [`notify_app::SyncBacklog`] like `GET /sync`.

use std::time::Duration;

use actix_web::{HttpRequest, HttpResponse, get, web};
use actix_web_lab::sse;
use futures::StreamExt as _;
use notify_app::{Caller, Credential, SyncBacklog, SyncRequest};
use notify_domain::{ChannelId, Cursor, DeviceId};
use notify_ports::{Event, PageRequest};
use serde::Deserialize;
use utoipa::IntoParams;

use crate::extract::{Authenticated, csv};
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// Short enough that a dead connection is noticed within a reconnect (design §8.1).
const KEEP_ALIVE: Duration = Duration::from_secs(25);

/// How far a slow client may fall behind before it is dropped; it then replays via
/// `Last-Event-ID` instead of the buffer growing unbounded.
const BACKPRESSURE: u32 = 64;

/// How often a live connection re-checks its credential (revocation, membership);
/// expiry is handled separately at `expires_at`.
const REAUTHENTICATE: Duration = Duration::from_secs(300);

/// Holds a §8.2 connection slot and `realtime_connections{transport}` (design §13) for
/// as long as it lives; released on drop so every exit path decrements.
struct Connected {
    metrics: std::sync::Arc<dyn notify_ports::Metrics>,
    transport: notify_ports::StreamKind,
    /// Released on drop; never read (see [`crate::slots::StreamSlot`]).
    _slot: crate::slots::StreamSlot,
}

impl Connected {
    /// Take a slot and start counting, or refuse with the `429` §8.2 asks for, before
    /// any backlog work.
    async fn open(
        state: &ApiState,
        caller: &Caller,
        transport: notify_ports::StreamKind,
    ) -> Result<Self, ApiError> {
        let refused = || {
            ApiError::from(notify_ports::Failure::RateLimited {
                retry_after_seconds: crate::slots::RETRY_AFTER_SECONDS,
            })
        };
        let mut slot = state
            .streams
            .acquire(&caller.principal)
            .ok_or_else(refused)?;
        if !state.streams.lease(&mut slot).await {
            return Err(refused());
        }

        state.metrics.stream_opened(transport);
        Ok(Self {
            metrics: std::sync::Arc::clone(&state.metrics),
            transport,
            _slot: slot,
        })
    }
}

impl Drop for Connected {
    fn drop(&mut self) {
        self.metrics.stream_closed(self.transport);
    }
}

/// Where to resume, and what to narrow to.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StreamParams {
    /// Comma-separated channel ids; absent means every channel the caller may read.
    pub channels: Option<String>,
    /// Resume after this cursor; `Last-Event-ID` wins when both are sent.
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

        let resume = last_event_id
            .or(params.cursor.as_deref())
            .map(str::parse::<Cursor>)
            .transpose()?;

        let cursor = state.stream_start.execute(&caller, resume, device).await?;

        Ok(Self {
            sync: state.sync.clone(),
            caller,
            channels,
            device,
            cursor,
        })
    }

    /// One page after the current cursor, advancing it; `true` when more follow. Callers
    /// loop page by page so an old cursor's backlog is never held in memory at once.
    async fn page(&mut self) -> Result<(Vec<Event>, bool), ApiError> {
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

        // Carry the per-reader `deferred_by_dnd` (§7.4) so clients needn't re-derive it.
        let muted = &page.deferred_by_dnd;
        let events = page
            .events
            .into_iter()
            .map(|mut event| {
                if let notify_ports::EventPayload::Revision(revision) = &event.payload
                    && muted.contains(&revision.notification_id)
                {
                    event.muted_by_dnd = true;
                }
                event
            })
            .collect();

        Ok((events, page.next.is_some()))
    }

    /// Re-check the credential the connection opened with; `false` ends the stream.
    async fn still_authorised(&mut self, state: &ApiState, credential: &Credential) -> bool {
        match state.authenticate.execute(credential.clone()).await {
            Ok(caller) => {
                self.caller = caller;
                true
            }
            Err(_) => false,
        }
    }

    /// Resolves when the stream's credential expires; never, for one that does not.
    async fn expired(&self, state: &ApiState) {
        match state.authenticate.time_left(&self.caller) {
            Some(left) => tokio::time::sleep(left).await,
            None => std::future::pending().await,
        }
    }
}

/// Render one event as SSE (design §8.1): `data:` is the whole event, channel included,
/// byte-identical to the WebSocket frame.
fn as_sse(event: &Event) -> Result<sse::Data, serde_json::Error> {
    Ok(sse::Data::new_json(event)?
        // The `id:` is the cursor, which `Last-Event-ID` replays.
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
        (status = 429, description = "Too many live connections held by this principal \
                                      on this replica (design §8.2)", body = Problem),
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

    // First, so the one refusal that should cost nothing never touches the store.
    let connected = Connected::open(&state, &caller.0, notify_ports::StreamKind::Sse).await?;
    let credential = credential_of(&request)?;
    let mut connection =
        Connection::open(&state, caller.0, &params, last_event_id.as_deref()).await?;
    let mut pokes = state.bus.subscribe().await?;

    let (sender, receiver) = tokio::sync::mpsc::channel(BACKPRESSURE as usize);
    let stream = sse::Sse::from_infallible_receiver(receiver).with_keep_alive(KEEP_ALIVE);

    // Backlog before live: the first page here (a refusal is still a status code), the
    // rest from the task one page at a time.
    let (first, mut more) = connection.page().await?;
    for event in &first {
        let data = as_sse(event).map_err(port_error)?;
        if sender.send(data.into()).await.is_err() {
            return Ok(stream);
        }
    }

    actix_web::rt::spawn(async move {
        // Moved into the task so the gauge drops however it ends.
        let _connected = connected;
        let mut reauthenticate = tokio::time::interval(REAUTHENTICATE);
        reauthenticate.tick().await;

        loop {
            while more {
                let events;
                (events, more) = match connection.page().await {
                    Ok(page) => page,
                    Err(error) => {
                        tracing::warn!(%error, "dropping a stream that cannot read the log");
                        return;
                    }
                };

                for event in events {
                    let Ok(data) = as_sse(&event) else {
                        tracing::error!(cursor = %event.cursor, "an event that will not serialise");
                        continue;
                    };
                    // Send failure: the client left, the ordinary end of a stream.
                    if sender.send(data.into()).await.is_err() {
                        return;
                    }
                }
            }

            tokio::select! {
                // Learn the client left without waiting for a publish.
                () = sender.closed() => return,
                () = connection.expired(&state) => return,
                _ = reauthenticate.tick() => {
                    if !connection.still_authorised(&state, &credential).await {
                        return;
                    }
                }
                poke = pokes.next() => {
                    if poke.is_none() {
                        return;
                    }
                    more = true;
                }
            }
        }
    });

    Ok(stream)
}

/// The credential a stream was opened with, kept to check it again later.
fn credential_of(request: &HttpRequest) -> Result<Credential, ApiError> {
    let header = request
        .headers()
        .get(actix_web::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    Ok(crate::extract::credential_from(header)?)
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
        (status = 429, description = "Too many live connections held by this principal \
                                      on this replica (design §8.2)", body = Problem),
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
    // Before the handshake, as for SSE.
    let connected = Connected::open(&state, &caller.0, notify_ports::StreamKind::WebSocket).await?;
    let credential = credential_of(&request)?;
    let mut connection = Connection::open(&state, caller.0, &params, None).await?;
    let mut pokes = state.bus.subscribe().await?;

    let (response, mut session, mut incoming) = match actix_ws::handle(&request, body) {
        Ok(parts) => parts,
        Err(error) => {
            // No upgrade sent: a request problem, not ours.
            tracing::debug!(%error, "websocket handshake refused");
            return Err(notify_domain::DomainError::Malformed {
                field: "upgrade",
                reason: "not a WebSocket handshake",
            }
            .into());
        }
    };

    actix_web::rt::spawn(async move {
        let _connected = connected;
        let mut reauthenticate = tokio::time::interval(REAUTHENTICATE);
        reauthenticate.tick().await;

        // Same order as SSE: backlog first, then live, one page at a time.
        let mut more = true;
        'live: loop {
            while more {
                let events;
                (events, more) = match connection.page().await {
                    Ok(page) => page,
                    Err(error) => {
                        tracing::warn!(%error, "dropping a socket that cannot read the log");
                        break 'live;
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
            }

            tokio::select! {
                // Read incoming frames to learn the client left.
                message = incoming.next() => match message {
                    Some(Ok(actix_ws::Message::Ping(bytes))) => {
                        if session.pong(&bytes).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(actix_ws::Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
                () = connection.expired(&state) => break,
                _ = reauthenticate.tick() => {
                    if !connection.still_authorised(&state, &credential).await {
                        break;
                    }
                }
                poke = pokes.next() => {
                    if poke.is_none() {
                        break;
                    }
                    more = true;
                }
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
