//! The live transports, SSE and WebSocket (design §8.1, ADR-0001). A poke only wakes
//! the connection, which reads rows via [`notify_app::SyncBacklog`] like `GET /sync`.

use std::time::Duration;

use actix_web::{HttpRequest, HttpResponse, get, web};
use actix_web_lab::sse;
use futures::{FutureExt as _, StreamExt as _};
use notify_app::{Caller, Credential, SyncBacklog, SyncRequest};
use notify_domain::{ChannelId, Cursor, DeviceId, Principal};
use notify_ports::{Event, PageRequest, Poke};
use serde::Deserialize;
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

/// The most queued pokes one wake swallows; whatever is left wakes the next read.
const DRAIN: usize = 1_024;

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
    /// What the caller may read, so a poke elsewhere costs nothing; refreshed on
    /// re-authentication.
    readable: Vec<ChannelId>,
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

        Ok(Self {
            sync: state.sync.clone(),
            caller,
            channels,
            device,
            cursor,
            readable,
        })
    }

    /// Whether `poke` may carry something this connection has not read yet.
    fn wants(&self, poke: &Poke) -> bool {
        match poke {
            Poke::Resync => true,
            Poke::Channel { id, cursor } => {
                *cursor > self.cursor
                    && self.readable.contains(id)
                    && (self.channels.is_empty() || self.channels.contains(id))
            }
            Poke::User { id, cursor } => {
                *cursor > self.cursor && self.caller.principal == Principal::User { id: *id }
            }
        }
    }

    /// Wait for a poke worth reading, then swallow those already queued behind it: a
    /// burst of publications costs one read, not one per publication. `None` when the
    /// bus is gone.
    async fn next_wake(&self, pokes: &mut futures::stream::BoxStream<'static, Poke>) -> Option<()> {
        loop {
            let poke = pokes.next().await?;
            if self.wants(&poke) {
                break;
            }
        }
        // Bounded, so a bus that never goes quiet cannot keep this from yielding.
        for _ in 0..DRAIN {
            match pokes.next().now_or_never() {
                Some(queued) => queued?,
                None => break,
            };
        }
        Some(())
    }

    /// One page after the current cursor, advancing it; `true` when more follow. Callers
    /// loop page by page so an old cursor's backlog is never held in memory at once.
    async fn page(&mut self) -> Result<(Vec<Event>, bool), ApiError> {
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
        let connected = connected;
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
                    if !sent(sender.send(data.into())).await {
                        return;
                    }
                }
            }

            tokio::select! {
                // Learn the client left without waiting for a publish.
                () = sender.closed() => return,
                () = connected.slot.kicked() => return,
                () = connection.expired(&state) => return,
                _ = reauthenticate.tick() => {
                    let before = connection.readable.clone();
                    if !connection.still_authorised(&state, &credential).await {
                        return;
                    }
                    // A channel joined since may already hold events no poke will repeat.
                    more = connection.readable.iter().any(|channel| !before.contains(channel));
                }
                woke = connection.next_wake(&mut pokes) => {
                    if woke.is_none() {
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
        'live: loop {
            // Nothing reads the socket while a backlog drains, so a pong that
            // arrives meanwhile waits unread — and a drain longer than
            // `PONG_TIMEOUT` would have a healthy client dropped by the first
            // heartbeat after it. The drain is our silence, not theirs: it does
            // not count. (Each write is still bounded by `SEND_TIMEOUT`, so a
            // peer that is truly gone is not kept alive by this.)
            let draining = tokio::time::Instant::now();
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
                    if !sent(session.text(frame)).await {
                        return;
                    }
                }
            }
            last_heard = last_heard
                .checked_add(draining.elapsed())
                .unwrap_or(last_heard);

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
                () = connection.expired(&state) => break,
                () = connected.slot.kicked() => break,
                _ = reauthenticate.tick() => {
                    let before = connection.readable.clone();
                    if !connection.still_authorised(&state, &credential).await {
                        break;
                    }
                    // A channel joined since may already hold events no poke will repeat.
                    more = connection.readable.iter().any(|channel| !before.contains(channel));
                }
                woke = connection.next_wake(&mut pokes) => {
                    if woke.is_none() {
                        break;
                    }
                    more = true;
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
fn port_error(error: serde_json::Error) -> ApiError {
    notify_ports::PortError::corrupt("event.payload", error).into()
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
        let store = InMemory::new();
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
        }
    }

    #[test]
    fn only_a_poke_that_can_carry_something_unread_wakes_a_stream() {
        let me = UserId::generate();
        let connection = connection(me);
        let channel = |id: ChannelId, at| Poke::Channel {
            id,
            cursor: Cursor::from_sequence(at),
        };
        let user = |id, at| Poke::User {
            id,
            cursor: Cursor::from_sequence(at),
        };

        assert!(connection.wants(&channel(ci(), 11)));
        assert!(connection.wants(&user(me, 11)));
        assert!(connection.wants(&Poke::Resync));

        assert!(!connection.wants(&channel(ci(), 10)), "already read");
        assert!(
            !connection.wants(&channel(ChannelId::parse("payroll").unwrap(), 11)),
            "not readable"
        );
        assert!(
            !connection.wants(&user(UserId::generate(), 11)),
            "somebody else"
        );
        assert!(!connection.wants(&user(me, 9)), "already read");
    }

    #[test]
    fn a_narrowed_stream_ignores_its_other_readable_channels() {
        let mut connection = connection(UserId::generate());
        connection
            .readable
            .push(ChannelId::parse("payroll").unwrap());
        connection.channels = vec![ci()];

        assert!(!connection.wants(&Poke::Channel {
            id: ChannelId::parse("payroll").unwrap(),
            cursor: Cursor::from_sequence(11),
        }));
    }

    #[tokio::test]
    async fn a_burst_of_pokes_is_one_wake() {
        let connection = connection(UserId::generate());
        let burst: Vec<Poke> = (11..20)
            .map(|at| Poke::Channel {
                id: ci(),
                cursor: Cursor::from_sequence(at),
            })
            .collect();
        // A live bus never ends; `pending` keeps the tail open as it would.
        let mut pokes = futures::stream::iter(burst)
            .chain(futures::stream::pending())
            .boxed();

        assert!(connection.next_wake(&mut pokes).await.is_some());
        assert!(pokes.next().now_or_never().flatten().is_none(), "drained");
    }
}
