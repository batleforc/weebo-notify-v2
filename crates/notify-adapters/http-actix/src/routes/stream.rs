//! The live transports, SSE and WebSocket (design §8.1, ADR-0001). Live events come from
//! the replica's [`notify_app::LiveFeed`], one read for every stream; a stream that
//! joined late or fell behind reads via [`notify_app::SyncBacklog`] like `GET /sync`.

use std::sync::Arc;
use std::time::Duration;

use actix_web::{HttpRequest, HttpResponse, get, web};
use actix_web_lab::sse;
use futures::StreamExt as _;
use notify_app::{Caller, Credential, Live, LiveBatch, SyncBacklog, SyncRequest};
use notify_domain::{ChannelId, Cursor, DeviceId, Dnd, Principal};
use notify_ports::{Audience, Event, EventPayload, PageRequest};
use serde::Deserialize;
use tokio::sync::broadcast;
use utoipa::IntoParams;

use crate::extract::{Authenticated, csv};
use crate::problem::{ApiError, Problem};
use crate::state::ApiState;

/// Short enough that a dead connection is noticed within a reconnect (design §8.1).
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// How far a slow client may fall behind before it is dropped; it then replays via
/// `Last-Event-ID` instead of the buffer growing unbounded.
const BACKPRESSURE: u32 = 64;

/// How often a live connection re-checks its credential (revocation, membership);
/// expiry is handled separately at `expires_at`. Bounds how long a revoked key's open
/// stream outlives its revocation (ADR-0023).
const REAUTHENTICATE: Duration = Duration::from_secs(60);

/// How often a live stream records its device's cursor; the rest is written when the
/// stream ends. A crash loses at most this much, which the client replays and dedups.
const ADVANCE_EVERY: Duration = Duration::from_secs(30);

/// How long one write may wait on a client that stopped reading; past it the client is
/// dropped, so it cannot hold a slot (or a revoked credential) by not reading.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Silence (not even a pong) after which a WebSocket peer is presumed gone: two pings plus slack.
const PONG_TIMEOUT: Duration = Duration::from_secs(60);

/// A write that completed within [`SEND_TIMEOUT`]; `false` ends the connection either way.
async fn sent<T, E>(write: impl core::future::Future<Output = Result<T, E>>) -> bool {
    matches!(tokio::time::timeout(SEND_TIMEOUT, write).await, Ok(Ok(_)))
}

/// Holds a §8.2 connection slot and `realtime_connections{transport}` (design §13) for
/// as long as it lives; released on drop so every exit path decrements. Eventually
/// accurate: SSE notices a departure within ~2 [`KEEP_ALIVE`]s, WebSocket on close or after
/// [`PONG_TIMEOUT`], and both drop a client that stops reading after [`SEND_TIMEOUT`].
struct Connected {
    metrics: std::sync::Arc<dyn notify_ports::Metrics>,
    transport: notify_ports::StreamKind,
    /// Released on drop; also says when its owner disconnects it.
    slot: crate::slots::StreamSlot,
}

impl Connected {
    /// Take a slot and start counting, or refuse with the `429` §8.2 asks for, before
    /// any backlog work.
    async fn open(
        state: &ApiState,
        caller: &Caller,
        transport: notify_ports::StreamKind,
        request: &HttpRequest,
        device: Option<&str>,
    ) -> Result<Self, ApiError> {
        let full = || {
            ApiError::from(notify_ports::Failure::RateLimited {
                retry_after_seconds: crate::slots::RETRY_AFTER_SECONDS,
            })
        };
        let mut slot = state.streams.acquire(&caller.principal).ok_or_else(full)?;
        let lease = lease_request(state, caller, transport, request, device);
        match state.streams.lease(&mut slot, lease).await {
            Ok(()) => {}
            Err(crate::slots::Refusal::Full) => return Err(full()),
            Err(crate::slots::Refusal::Disconnected) => {
                return Err(notify_domain::DomainError::StreamDisconnected.into());
            }
        }

        state.metrics.stream_opened(transport);
        Ok(Self {
            metrics: std::sync::Arc::clone(&state.metrics),
            transport,
            slot,
        })
    }
}

/// What the lease table records about a connection (its owner lists and disconnects them).
fn lease_request(
    state: &ApiState,
    caller: &Caller,
    transport: notify_ports::StreamKind,
    request: &HttpRequest,
    device: Option<&str>,
) -> notify_ports::LeaseRequest {
    let header = |name| {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    };
    let credential = match (&caller.grant, device) {
        (notify_app::Grant::ApiKey { .. }, _) => "key".to_owned(),
        (notify_app::Grant::Federated { .. }, _) => "ci".to_owned(),
        (_, Some(device)) => format!("device:{device}"),
        _ => "session".to_owned(),
    };
    notify_ports::LeaseRequest {
        principal: caller.principal.reference(),
        replica: state.streams.replica().unwrap_or_default().to_owned(),
        transport: match transport {
            notify_ports::StreamKind::Sse => "sse",
            notify_ports::StreamKind::WebSocket => "websocket",
        }
        .to_owned(),
        client: header(actix_web::http::header::USER_AGENT)
            .chars()
            .take(160)
            .collect(),
        credential,
        fingerprint: fingerprint(header(actix_web::http::header::AUTHORIZATION)),
    }
}

/// A hash of the presented credential: recognises it again without storing it.
pub(crate) fn fingerprint(authorization: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(authorization.as_bytes()))
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
    /// What the caller may read, so the feed's other events are skipped; refreshed on
    /// re-authentication.
    readable: Vec<ChannelId>,
    /// The caller's do-not-disturb, re-read when a preference of theirs goes by.
    dnd: Option<Dnd>,
    /// The device whose cursor live events advance, when this stream may move it.
    device_live: Option<DeviceId>,
    /// The furthest live event delivered and not yet recorded on the device.
    unsaved: Option<Cursor>,
    saved_at: tokio::time::Instant,
}

/// What the feed asks of a stream.
enum Wake {
    /// Send these, already filtered for this reader.
    Send(Vec<Outgoing>),
    /// Read the log for yourself: a gap, a lag, or channels to bring in.
    Read,
}

/// An event on its way out: one of the feed's, sent as the frame it rendered once for every
/// stream, or this reader's own (a page it read, or a feed event it marks muted).
enum Outgoing {
    Fed(Arc<LiveBatch>, usize),
    Own(Box<Event>),
}

impl Outgoing {
    fn event(&self) -> Option<&Event> {
        match self {
            Self::Fed(batch, index) => batch.events.get(*index),
            Self::Own(event) => Some(event),
        }
    }

    /// The JSON both transports carry; `None` for an event that will not serialise, our
    /// bug, which is logged and skipped rather than the client's end.
    fn json(&self) -> Option<String> {
        match self {
            // The feed logged a missing frame once, rather than once per stream here.
            Self::Fed(batch, index) => batch.frames.get(*index)?.as_deref().map(str::to_owned),
            Self::Own(event) => serde_json::to_string(event)
                .inspect_err(|error| {
                    tracing::error!(%error, cursor = %event.cursor, "an event that will not serialise");
                })
                .ok(),
        }
    }
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
        // Once per connection, not per page: every poke would otherwise write it again.
        state.sync.remember_groups(&caller).await?;
        let readable = state.sync.readable_channels(&caller).await?;
        let dnd = state.sync.dnd(&caller).await?;
        let device_live = state.sync.live_device(&caller, device, &channels).await?;

        Ok(Self {
            sync: state.sync.clone(),
            caller,
            channels,
            device,
            cursor,
            readable,
            dnd,
            device_live,
            unsaved: None,
            saved_at: tokio::time::Instant::now(),
        })
    }

    /// Whether this reader sees `event`: the same rule `SyncBacklog::page` applies.
    fn sees(&self, event: &Event) -> bool {
        match &event.audience {
            Audience::Channel { id } => {
                self.readable.contains(id)
                    && (self.channels.is_empty() || self.channels.contains(id))
            }
            // Private events ignore `?channels=` (T26), except where a page read returns
            // nothing at all: a narrowing or a key left with no channel.
            Audience::User { id } => {
                let narrowed = !self.channels.is_empty();
                let nothing = if narrowed {
                    !self
                        .channels
                        .iter()
                        .any(|channel| self.readable.contains(channel))
                } else {
                    self.readable.is_empty()
                };
                self.is_me(*id)
                    && !(nothing
                        && (narrowed || self.caller.grant != notify_app::Grant::Membership))
            }
        }
    }

    fn is_me(&self, id: notify_domain::UserId) -> bool {
        self.caller.principal == Principal::User { id }
    }

    /// Re-read what the caller may read, after a membership changed. A failure keeps the
    /// old set: the re-authentication tick tries again.
    async fn refresh_readable(&mut self) {
        match self.sync.readable_channels(&self.caller).await {
            Ok(readable) => self.readable = readable,
            Err(error) => tracing::warn!(%error, "could not refresh a stream's channels"),
        }
    }

    /// Wait for live events this reader sees, or a reason to read for itself. `None`
    /// when the feed is gone.
    async fn next_live(&mut self, live: &mut broadcast::Receiver<Live>) -> Option<Wake> {
        loop {
            match live.recv().await {
                Ok(Live::User(id)) if self.is_me(id) => {
                    // A membership may have added a channel whose events went by unread.
                    self.refresh_readable().await;
                    return Some(Wake::Read);
                }
                Ok(Live::User(_)) => {}
                Ok(Live::Batch(batch)) => {
                    if batch.until <= self.cursor {
                        continue;
                    }
                    if batch.after > self.cursor {
                        return Some(Wake::Read);
                    }
                    let events = self.take(batch).await;
                    if !events.is_empty() {
                        return Some(Wake::Send(events));
                    }
                }
                // Fell behind the feed: whatever was dropped is still in the log.
                Err(broadcast::error::RecvError::Lagged(_)) => return Some(Wake::Read),
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// This reader's share of a batch that starts at or before its cursor, which then
    /// moves to the batch's end: what it skipped was not its to see.
    async fn take(&mut self, batch: Arc<LiveBatch>) -> Vec<Outgoing> {
        let kept: Vec<usize> = batch
            .events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.cursor > self.cursor && self.sees(event))
            .map(|(index, _)| index)
            .collect();
        let events = || kept.iter().filter_map(|index| batch.events.get(*index));
        self.cursor = batch.until;

        let mine = |event: &Event| matches!(event.payload, EventPayload::Preference(_));
        if events().any(mine) {
            match self.sync.dnd(&self.caller).await {
                Ok(dnd) => self.dnd = dnd,
                Err(error) => tracing::warn!(%error, "could not re-read a stream's do-not-disturb"),
            }
        }
        // Carry the per-reader `deferred_by_dnd` (§7.4) so clients needn't re-derive it;
        // only a muted event is copied and rendered again, for this reader.
        let muted = self
            .dnd
            .as_ref()
            .map(|dnd| notify_app::deferred_by(dnd, events()))
            .unwrap_or_default();

        if let Some(last) = events().next_back() {
            self.unsaved = Some(last.cursor);
            if self.saved_at.elapsed() >= ADVANCE_EVERY {
                self.save_device().await;
            }
        }

        kept.into_iter()
            .map(|index| match batch.events.get(index) {
                Some(event) if is_muted(event, &muted) => Outgoing::Own(Box::new(Event {
                    muted_by_dnd: true,
                    ..event.clone()
                })),
                _ => Outgoing::Fed(batch.clone(), index),
            })
            .collect()
    }

    /// Record the device's cursor now rather than at the next [`ADVANCE_EVERY`].
    async fn save_device(&mut self) {
        let (Some(device), Some(cursor)) = (self.device_live, self.unsaved.take()) else {
            return;
        };
        self.saved_at = tokio::time::Instant::now();
        if let Err(error) = self.sync.advance_device(device, cursor).await {
            tracing::warn!(%error, "could not record a stream's device cursor");
        }
    }

    /// One page after the current cursor, advancing it; `true` when more follow. Callers
    /// loop page by page so an old cursor's backlog is never held in memory at once.
    ///
    /// `fed` is the feed's position, taken before the read: once caught up, this stream has
    /// read everything the feed had sent, and joins it there.
    async fn page(&mut self, fed: Cursor) -> Result<(Vec<Outgoing>, bool), ApiError> {
        let page = self
            .sync
            .page(
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
        if page.next.is_none() {
            self.cursor = self.cursor.max(fed);
        }

        // Carry the per-reader `deferred_by_dnd` (§7.4) so clients needn't re-derive it.
        let muted = &page.deferred_by_dnd;
        let events = page
            .events
            .into_iter()
            .map(|mut event| {
                event.muted_by_dnd = is_muted(&event, muted);
                Outgoing::Own(Box::new(event))
            })
            .collect();

        Ok((events, page.next.is_some()))
    }

    /// Re-check the credential the connection opened with; `false` ends the stream.
    async fn still_authorised(&mut self, state: &ApiState, credential: &Credential) -> bool {
        match state.authenticate.execute(credential.clone()).await {
            Ok(caller) => {
                // Membership may have changed: a channel joined since must wake us too.
                let readable = match self.sync.readable_channels(&caller).await {
                    Ok(readable) => readable,
                    Err(error) => {
                        // Ends the stream; the client reconnects and re-reads from its cursor.
                        tracing::warn!(%error, "could not refresh a stream's channels");
                        return false;
                    }
                };
                self.readable = readable;
                self.caller = caller;
                true
            }
            Err(_) => false,
        }
    }

    /// Resolves when the stream's credential expires; never, for one that does not.
    /// Not borrowing the connection, which the feed's arm of the same `select!` moves on.
    fn expired(&self, state: &ApiState) -> impl Future<Output = ()> + use<> {
        let left = state.authenticate.time_left(&self.caller);
        async move {
            match left {
                Some(left) => tokio::time::sleep(left).await,
                None => std::future::pending().await,
            }
        }
    }
}

/// Whether `event` is a revision of a notification this reader's DND deferred (§7.4).
fn is_muted(event: &Event, muted: &[notify_domain::NotificationId]) -> bool {
    matches!(&event.payload, EventPayload::Revision(revision) if muted.contains(&revision.notification_id))
}

/// Write these as SSE; `false` once the client is gone.
async fn sent_sse(
    sender: &tokio::sync::mpsc::Sender<sse::Event>,
    events: impl IntoIterator<Item = Outgoing>,
) -> bool {
    for event in events {
        let Some(data) = as_sse(&event) else {
            continue;
        };
        if !sent(sender.send(data.into())).await {
            return false;
        }
    }
    true
}

/// [`sent_sse`], as WebSocket frames.
async fn sent_ws(
    session: &mut actix_ws::Session,
    events: impl IntoIterator<Item = Outgoing>,
) -> bool {
    for event in events {
        let Some(frame) = event.json() else {
            continue;
        };
        if !sent(session.text(frame)).await {
            return false;
        }
    }
    true
}

/// Whatever the stream's end, the device keeps the cursor it reached.
impl Drop for Connection {
    fn drop(&mut self) {
        let (Some(device), Some(cursor)) = (self.device_live, self.unsaved.take()) else {
            return;
        };
        let sync = self.sync.clone();
        actix_web::rt::spawn(async move {
            if let Err(error) = sync.advance_device(device, cursor).await {
                tracing::warn!(%error, "could not record a closed stream's device cursor");
            }
        });
    }
}

/// Render one event as SSE (design §8.1): `data:` is the whole event, channel included,
/// byte-identical to the WebSocket frame.
fn as_sse(outgoing: &Outgoing) -> Option<sse::Data> {
    let event = outgoing.event()?;
    Some(
        sse::Data::new(outgoing.json()?)
            // The `id:` is the cursor, which `Last-Event-ID` replays.
            .id(event.cursor.to_string())
            .event(event.kind().as_str()),
    )
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
    let connected = Connected::open(
        &state,
        &caller.0,
        notify_ports::StreamKind::Sse,
        &request,
        params.device.as_deref(),
    )
    .await?;
    let credential = credential_of(&request)?;
    let mut connection =
        Connection::open(&state, caller.0, &params, last_event_id.as_deref()).await?;
    // Before the first read, so nothing the feed sends after it is missed.
    let mut live = state.live.subscribe();

    let (sender, receiver) = tokio::sync::mpsc::channel(BACKPRESSURE as usize);
    let stream = sse::Sse::from_infallible_receiver(receiver).with_keep_alive(KEEP_ALIVE);

    // Backlog before live: the first page here (a refusal is still a status code), the
    // rest from the task one page at a time.
    let (first, mut more) = connection.page(state.live.position()).await?;
    for event in &first {
        let data = as_sse(event).ok_or_else(unserialisable)?;
        if sender.send(data.into()).await.is_err() {
            return Ok(stream);
        }
    }

    actix_web::rt::spawn(async move {
        // Moved into the task so the gauge drops however it ends.
        let connected = connected;
        let mut reauthenticate = tokio::time::interval(REAUTHENTICATE);
        reauthenticate.tick().await;
        let mut pending: Vec<Outgoing> = Vec::new();

        loop {
            // Send failure: the client left, the ordinary end of a stream.
            if !sent_sse(&sender, pending.drain(..)).await {
                return;
            }
            while more {
                let events;
                (events, more) = match connection.page(state.live.position()).await {
                    Ok(page) => page,
                    Err(error) => {
                        tracing::warn!(%error, "dropping a stream that cannot read the log");
                        return;
                    }
                };

                if !sent_sse(&sender, events).await {
                    return;
                }
            }

            let expiry = connection.expired(&state);
            tokio::select! {
                // Learn the client left without waiting for a publish.
                () = sender.closed() => return,
                () = connected.slot.kicked() => return,
                () = expiry => return,
                _ = reauthenticate.tick() => {
                    let before = connection.readable.clone();
                    if !connection.still_authorised(&state, &credential).await {
                        return;
                    }
                    // A channel joined since may already hold events no poke will repeat.
                    more = connection.readable.iter().any(|channel| !before.contains(channel));
                }
                woke = connection.next_live(&mut live) => match woke {
                    None => return,
                    Some(Wake::Read) => more = true,
                    Some(Wake::Send(events)) => pending = events,
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
    let connected = Connected::open(
        &state,
        &caller.0,
        notify_ports::StreamKind::WebSocket,
        &request,
        params.device.as_deref(),
    )
    .await?;
    let credential = credential_of(&request)?;
    let mut connection = Connection::open(&state, caller.0, &params, None).await?;
    // Before the first read, so nothing the feed sends after it is missed.
    let mut live = state.live.subscribe();

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
        let connected = connected;
        let mut reauthenticate = tokio::time::interval(REAUTHENTICATE);
        reauthenticate.tick().await;
        // SSE has its keep-alive comment; a socket has pings. They also find a peer
        // that vanished without a word, which reading alone never would.
        let mut heartbeat = tokio::time::interval(KEEP_ALIVE);
        heartbeat.tick().await;
        let mut last_heard = tokio::time::Instant::now();

        // Same order as SSE: backlog first, then live, one page at a time.
        let mut more = true;
        let mut pending: Vec<Outgoing> = Vec::new();
        'live: loop {
            if !sent_ws(&mut session, pending.drain(..)).await {
                return;
            }
            // Nothing reads the socket while a backlog drains, so a pong that
            // arrives meanwhile waits unread — and a drain longer than
            // `PONG_TIMEOUT` would have a healthy client dropped by the first
            // heartbeat after it. The drain is our silence, not theirs: it does
            // not count. (Each write is still bounded by `SEND_TIMEOUT`, so a
            // peer that is truly gone is not kept alive by this.)
            let draining = tokio::time::Instant::now();
            while more {
                let events;
                (events, more) = match connection.page(state.live.position()).await {
                    Ok(page) => page,
                    Err(error) => {
                        tracing::warn!(%error, "dropping a socket that cannot read the log");
                        break 'live;
                    }
                };

                if !sent_ws(&mut session, events).await {
                    return;
                }
            }
            last_heard = last_heard
                .checked_add(draining.elapsed())
                .unwrap_or(last_heard);

            let expiry = connection.expired(&state);
            tokio::select! {
                // Read incoming frames to learn the client left.
                message = incoming.next() => match message {
                    Some(Ok(actix_ws::Message::Ping(bytes))) => {
                        last_heard = tokio::time::Instant::now();
                        if !sent(session.pong(&bytes)).await {
                            return;
                        }
                    }
                    Some(Ok(actix_ws::Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => last_heard = tokio::time::Instant::now(),
                },
                _ = heartbeat.tick() => {
                    if last_heard.elapsed() > PONG_TIMEOUT || !sent(session.ping(b"")).await {
                        break;
                    }
                }
                () = expiry => break,
                () = connected.slot.kicked() => break,
                _ = reauthenticate.tick() => {
                    let before = connection.readable.clone();
                    if !connection.still_authorised(&state, &credential).await {
                        break;
                    }
                    // A channel joined since may already hold events no poke will repeat.
                    more = connection.readable.iter().any(|channel| !before.contains(channel));
                }
                woke = connection.next_live(&mut live) => match woke {
                    None => break,
                    Some(Wake::Read) => more = true,
                    Some(Wake::Send(events)) => pending = events,
                }
            }
        }

        #[expect(
            clippy::let_underscore_must_use,
            reason = "bounded too, as a peer that stopped reading would hold the close; the stream ends either way"
        )]
        let _ = tokio::time::timeout(SEND_TIMEOUT, session.close(None)).await;
    });

    Ok(response)
}

/// An event we cannot serialise is our bug, not the caller's.
fn unserialisable() -> ApiError {
    notify_ports::PortError::corrupt("event.payload", "an event that will not serialise").into()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use notify_app::Authorizer;
    use notify_domain::UserId;
    use notify_ports::test_utils::{InMemory, TestClock};

    use super::*;

    fn ci() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn connection(user: UserId) -> Connection {
        connection_on(&InMemory::new(), user)
    }

    fn connection_on(store: &InMemory, user: UserId) -> Connection {
        let store = store.clone();
        Connection {
            sync: SyncBacklog::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Authorizer::new(Arc::new(store)),
                Arc::new(TestClock::new()),
            ),
            caller: Caller::member(Principal::User { id: user }),
            channels: Vec::new(),
            device: None,
            cursor: Cursor::from_sequence(10),
            readable: vec![ci()],
            dnd: None,
            device_live: None,
            unsaved: None,
            saved_at: tokio::time::Instant::now(),
        }
    }

    fn event(at: u64, audience: Audience) -> Event {
        Event {
            cursor: Cursor::from_sequence(at),
            audience,
            recorded_at: time::OffsetDateTime::UNIX_EPOCH,
            muted_by_dnd: false,
            payload: EventPayload::Tombstone {
                notification_id: notify_domain::NotificationId::generate(),
                reason: notify_domain::TombstoneReason::Expired,
            },
        }
    }

    fn on(channel: &str) -> Audience {
        Audience::Channel {
            id: ChannelId::parse(channel).unwrap(),
        }
    }

    fn batch(after: u64, events: Vec<Event>) -> Live {
        let until = events
            .last()
            .map_or(Cursor::from_sequence(after), |event| event.cursor);
        let frames = events
            .iter()
            .map(|event| serde_json::to_string(event).ok().map(Into::into))
            .collect();
        Live::Batch(Arc::new(LiveBatch {
            after: Cursor::from_sequence(after),
            until,
            events,
            frames,
        }))
    }

    /// A feed with these messages already queued; hold the sender, or the feed is closed.
    fn feed(messages: Vec<Live>) -> (broadcast::Sender<Live>, broadcast::Receiver<Live>) {
        let (sender, receiver) = broadcast::channel(16);
        for message in messages {
            sender.send(message).unwrap();
        }
        (sender, receiver)
    }

    fn cursors(wake: Option<Wake>) -> Vec<Cursor> {
        match wake {
            Some(Wake::Send(events)) => events
                .iter()
                .filter_map(|event| Some(event.event()?.cursor))
                .collect(),
            Some(Wake::Read) => panic!("expected events, got a read"),
            None => panic!("expected events, got the end"),
        }
    }

    fn at(sequences: &[u64]) -> Vec<Cursor> {
        sequences
            .iter()
            .copied()
            .map(Cursor::from_sequence)
            .collect()
    }

    #[tokio::test]
    async fn a_stream_keeps_only_what_its_reader_sees_and_moves_to_the_batch_end() {
        let me = UserId::generate();
        let mut connection = connection(me);
        let (_feed, mut live) = feed(vec![batch(
            10,
            vec![
                event(11, on("ci-infra")),
                event(12, on("payroll")),
                event(
                    13,
                    Audience::User {
                        id: UserId::generate(),
                    },
                ),
                event(14, Audience::User { id: me }),
            ],
        )]);

        assert_eq!(
            cursors(connection.next_live(&mut live).await),
            at(&[11, 14])
        );
        assert_eq!(connection.cursor, Cursor::from_sequence(14));
    }

    #[tokio::test]
    async fn a_narrowed_stream_ignores_its_other_readable_channels() {
        let mut connection = connection(UserId::generate());
        connection
            .readable
            .push(ChannelId::parse("payroll").unwrap());
        connection.channels = vec![ci()];
        let (_feed, mut live) = feed(vec![batch(
            10,
            vec![event(11, on("payroll")), event(12, on("ci-infra"))],
        )]);

        assert_eq!(cursors(connection.next_live(&mut live).await), at(&[12]));
    }

    #[tokio::test]
    async fn a_batch_already_read_is_skipped_and_one_past_a_gap_asks_for_a_read() {
        let mut connection = connection(UserId::generate());
        let (_feed, mut live) = feed(vec![
            batch(5, vec![event(8, on("ci-infra")), event(10, on("ci-infra"))]),
            batch(12, vec![event(13, on("ci-infra"))]),
        ]);

        assert!(
            matches!(connection.next_live(&mut live).await, Some(Wake::Read)),
            "11 and 12 went out before this stream caught up: the log has them, the feed no longer"
        );
        assert_eq!(
            connection.cursor,
            Cursor::from_sequence(10),
            "a read starts where we are"
        );
    }

    #[tokio::test]
    async fn a_batch_overlapping_the_cursor_delivers_only_what_follows_it() {
        let mut connection = connection(UserId::generate());
        let (_feed, mut live) = feed(vec![batch(
            8,
            vec![
                event(9, on("ci-infra")),
                event(10, on("ci-infra")),
                event(11, on("ci-infra")),
            ],
        )]);

        assert_eq!(cursors(connection.next_live(&mut live).await), at(&[11]));
    }

    #[tokio::test]
    async fn a_stream_that_fell_behind_the_feed_reads_for_itself() {
        let mut connection = connection(UserId::generate());
        let (sender, mut live) = broadcast::channel(1);
        sender
            .send(batch(10, vec![event(11, on("ci-infra"))]))
            .unwrap();
        sender
            .send(batch(11, vec![event(12, on("ci-infra"))]))
            .unwrap();

        assert!(matches!(
            connection.next_live(&mut live).await,
            Some(Wake::Read)
        ));
    }

    #[tokio::test]
    async fn the_feed_frame_goes_out_as_is_unless_this_reader_muted_the_event() {
        let me = UserId::generate();
        let mut connection = connection(me);
        connection.dnd = Some(
            notify_domain::DndDraft {
                until: Some(time::OffsetDateTime::now_utc() + time::Duration::days(1)),
                ..notify_domain::DndDraft::default()
            }
            .for_user(me),
        );
        let (notification, written) = notify_domain::Notification::create(
            notify_domain::NotificationId::generate(),
            ci(),
            None,
            notify_domain::RevisionDraft::lifecycle(
                notify_domain::State::Open,
                notify_domain::ContentDraft::titled("quiet please"),
            ),
            notify_domain::Author::anonymous(Principal::User { id: me }),
            time::OffsetDateTime::now_utc(),
            &notify_domain::ChannelPolicy::default(),
        )
        .unwrap();
        let revision = Event {
            payload: EventPayload::Revision(Box::new(notify_ports::RevisionEvent::new(
                &notification,
                &written.revision,
            ))),
            ..event(12, on("ci-infra"))
        };
        let (_feed, mut live) = feed(vec![batch(10, vec![event(11, on("ci-infra")), revision])]);

        let Some(Wake::Send(sent)) = connection.next_live(&mut live).await else {
            panic!("expected events");
        };
        let [plain, muted] = sent.as_slice() else {
            panic!("expected two events");
        };
        assert!(matches!(plain, Outgoing::Fed(_, 0)), "shared, not copied");
        let Outgoing::Own(muted) = muted else {
            panic!("a muted event is this reader's own");
        };
        assert!(muted.muted_by_dnd);
    }

    #[tokio::test]
    async fn a_closed_feed_ends_the_stream() {
        let mut connection = connection(UserId::generate());
        let (sender, mut live) = broadcast::channel::<Live>(1);
        drop(sender);

        assert!(connection.next_live(&mut live).await.is_none());
    }

    #[tokio::test]
    async fn a_signal_addressed_to_the_reader_brings_in_a_channel_joined_since() {
        let me = UserId::generate();
        let store = InMemory::new();
        let mut connection = connection_on(&store, me);
        let joined = ChannelId::parse("payroll").unwrap();
        let mut channel = notify_domain::Channel::dynamic(joined.clone(), None).unwrap();
        channel.upsert_member(
            notify_domain::Membership::new(
                Principal::User { id: me },
                vec![notify_domain::Scope::Subscribe],
            )
            .unwrap(),
        );
        store.seed_channel(channel);
        let (_feed, mut live) = feed(vec![Live::User(UserId::generate()), Live::User(me)]);

        assert!(
            matches!(connection.next_live(&mut live).await, Some(Wake::Read)),
            "its events went by before the membership: only a read finds them"
        );
        assert!(connection.readable.contains(&joined));
    }
}
