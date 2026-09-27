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
use notify_app::{Caller, Credential, SyncBacklog, SyncRequest};
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

/// How often a live connection re-checks the credential it opened with.
///
/// Expiry needs no re-check: the stream closes at the credential's own
/// `expires_at`. This catches revocation and membership changes, which can
/// therefore lag by up to this much.
const REAUTHENTICATE: Duration = Duration::from_secs(300);

/// Holds a §8.2 connection slot and `realtime_connections{transport}` up for as
/// long as it lives (design §13).
///
/// One guard for both because they have exactly the same lifetime and exactly the
/// same hard part — the decrement. Two guards would be two chances to get that
/// wrong, and the failure modes differ in severity: a leaked gauge misleads an
/// operator, a leaked slot locks somebody out of their own inbox until the replica
/// restarts.
///
/// A guard and not two calls, because the interesting half is the decrement: a
/// stream ends when the client goes away, when the task is aborted at shutdown, or
/// when the log becomes unreadable — and a handler that decremented on its own
/// success path would leak a connection on every one of those. The gauge also feeds
/// the HPA (§11), so a leak there is an instance that never scales back down.
///
/// It is **eventually** accurate, and how long "eventually" is differs by transport
/// — which is worth knowing before reading the gauge as a live number:
///
/// * **SSE** waits on the receiver being dropped as well as on the next poke, so a
///   departure is learned without anything being published. The remaining lag is the
///   responder's: it drops its receiver on the first write the socket actually
///   refuses, and the socket refuses one write later than one would expect — the
///   kernel absorbs the first. Measured on a killed client with nothing in flight:
///   **off the gauge 50 s later**, two [`KEEP_ALIVE`]s, no publish involved;
/// * **WebSocket** reads the incoming stream, so a client that closes, or a
///   connection that errors, comes off as soon as the frame or the error arrives.
///   A peer that vanishes without a word is still only found by the next write.
///
/// Measured rather than assumed, and the measurement is why the select below exists:
/// waiting on pokes alone, a killed client survived a keep-alive *and* a first
/// publish, and only came off the gauge at the **second** — which on a quiet channel
/// is however long that takes.
struct Connected {
    metrics: std::sync::Arc<dyn notify_ports::Metrics>,
    transport: notify_ports::StreamKind,
    /// Released on drop; never read. Held rather than dropped immediately, which is
    /// the entire mechanism — see [`crate::slots::StreamSlot`].
    _slot: crate::slots::StreamSlot,
}

impl Connected {
    /// Take a slot and start counting, or refuse with the `429` §8.2 asks for.
    ///
    /// Refusing *here* rather than after the backlog is drained is deliberate: the
    /// twenty-first connection is turned away before it costs a page of events, and
    /// a client over its ceiling learns so from a status code instead of from a
    /// stream that goes quiet.
    fn open(
        state: &ApiState,
        caller: &Caller,
        transport: notify_ports::StreamKind,
    ) -> Result<Self, ApiError> {
        let slot = state.streams.acquire(&caller.principal).ok_or_else(|| {
            ApiError::from(notify_ports::Failure::RateLimited {
                retry_after_seconds: crate::slots::RETRY_AFTER_SECONDS,
            })
        })?;

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

        let cursor = state.stream_start.execute(&caller, resume, device).await?;

        Ok(Self {
            sync: state.sync.clone(),
            caller,
            channels,
            device,
            cursor,
        })
    }

    /// One page after the current cursor, advancing it; `true` when more follow.
    ///
    /// A poke says "at least one thing landed", never how much, so callers loop
    /// until this says there is no more — sending each page before asking for the
    /// next. Collecting the whole backlog first would hold an old cursor's entire
    /// visible log in memory before the first byte went out.
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

        // The answer to "should this make a noise for *this* reader" is right
        // here and used to be thrown away: the sync use case computes
        // `deferred_by_dnd` per caller (§7.4) and this loop kept only the
        // events. Every client then re-derived the rule for itself — three
        // implementations of it at the last count (§14.4, D22). Carrying it
        // costs a lookup per event and removes the reason they existed.
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

    /// Check the credential the connection opened with again.
    ///
    /// A stream outlives the request that authenticated it; without this, a token
    /// that expired, a key that was revoked or a membership that was removed would
    /// keep receiving for as long as the socket stayed up. `false` ends the stream.
    async fn still_authorised(&mut self, state: &ApiState, credential: &Credential) -> bool {
        match state.authenticate.execute(credential.clone()).await {
            Ok(caller) => {
                self.caller = caller;
                true
            }
            Err(_) => false,
        }
    }

    /// Resolves when the credential this stream runs on expires; never, for one
    /// that does not.
    async fn expired(&self, state: &ApiState) {
        match state.authenticate.time_left(&self.caller) {
            Some(left) => tokio::time::sleep(left).await,
            None => std::future::pending().await,
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

    // Counted from here, where the connection is real, rather than from the first
    // poke — a client that connects and never receives anything is still one this
    // replica is holding open. First of all, too: this is the one refusal that
    // should cost nothing, so the ceiling is checked before the store is touched.
    let connected = Connected::open(&state, &caller.0, notify_ports::StreamKind::Sse)?;
    let credential = credential_of(&request)?;
    let mut connection =
        Connection::open(&state, caller.0, &params, last_event_id.as_deref()).await?;
    let mut pokes = state.bus.subscribe().await?;

    let (sender, receiver) = tokio::sync::mpsc::channel(BACKPRESSURE as usize);
    let stream = sse::Sse::from_infallible_receiver(receiver).with_keep_alive(KEEP_ALIVE);

    // Everything the caller missed goes out before the first poke, so a reconnect
    // is seamless rather than "you get what happens from now on". The first page
    // here, so a refusal is still a status code; the rest from the task, one page
    // at a time — the channel holds exactly one page until the response is polled.
    let (first, mut more) = connection.page().await?;
    for event in &first {
        let data = as_sse(event).map_err(port_error)?;
        if sender.send(data.into()).await.is_err() {
            return Ok(stream);
        }
    }

    actix_web::rt::spawn(async move {
        // Moved into the task, so the gauge comes back down whichever way this ends:
        // the client going away, the log becoming unreadable, or the runtime
        // dropping the task at shutdown.
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
                    // A send failure is the client having gone away, which is the
                    // ordinary end of a stream rather than something to report.
                    if sender.send(data.into()).await.is_err() {
                        return;
                    }
                }
            }

            tokio::select! {
                // The client leaving, learned without waiting for somebody to
                // publish. Waiting only on pokes was measurably wrong: a killed
                // client stayed on the gauge through a keep-alive and a first
                // publish, and on a channel that is quiet overnight it would have
                // stayed there overnight — with the HPA reading the number.
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
    // Before the handshake, for the same reason as SSE: a client over its ceiling
    // gets a status code rather than an upgraded socket that closes on its own.
    let connected = Connected::open(&state, &caller.0, notify_ports::StreamKind::WebSocket)?;
    let credential = credential_of(&request)?;
    let mut connection = Connection::open(&state, caller.0, &params, None).await?;
    let mut pokes = state.bus.subscribe().await?;

    let (response, mut session, mut incoming) = match actix_ws::handle(&request, body) {
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
                // Read what the client sends, if only to learn it left: without
                // this a dead socket held its slot until the next publish.
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
