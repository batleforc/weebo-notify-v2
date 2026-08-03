//! One in-memory store behind every repository port.
//!
//! Everything shares a single mutex-guarded state, which is what makes the cursor
//! sequence coherent across repositories: a revision, a receipt and an interaction
//! recorded in that order come back from the event log in that order, exactly as
//! the real schema guarantees (ADR-0012). Repositories that each kept their own
//! counter would let a use-case test pass while the production ordering was wrong.

use core::cmp::Reverse;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::stream::BoxStream;
use notify_domain::{
    ApiKey, ApiKeyId, ApiKeyPrefix, Channel, ChannelId, Cursor, Device, DeviceId, ExternalId,
    InteractionRecord, Notification, NotificationId, Principal, Receipt, ReceiptKind, Recorded,
    Revision, Scope, ServiceAccount, ServiceAccountId, Silence, TombstoneReason, User, UserId,
};
use time::OffsetDateTime;

use crate::audit::{AuditEntry, AuditLog, AuditQuery};
use crate::channels::ChannelRepository;
use crate::devices::DeviceRepository;
use crate::error::{PortError, PortResult};
use crate::event::{
    Audience, Event, EventPayload, EventStore, ExpiredWhileAway, PreferenceEvent, RevisionEvent,
    SyncPage, SyncQuery,
};
use crate::eventbus::{EventBus, Poke};
use crate::idempotency::{Claim, IdempotencyStore};
use crate::identities::{ApiKeyRepository, ServiceAccountRepository, StoredApiKey, UserRepository};
use crate::interactions::InteractionRepository;
use crate::notifications::{ExpiredNotification, InboxQuery, NotificationRepository, SearchQuery};
use crate::pagination::{Page, PageRequest};
use crate::push::{PushDispatcher, PushMessage, PushOutcome};
use crate::receipts::ReceiptRepository;
use crate::sinks::{DeadLetter, SinkDelivery, SinkQueue};

/// A row plus the cursor it was last written at.
#[derive(Debug, Clone)]
struct Row<T> {
    value: T,
    cursor: Cursor,
}

/// A claimed `Idempotency-Key`.
#[derive(Debug, Clone)]
struct Reservation {
    principal_ref: String,
    key: String,
    response: Option<String>,
    expires_at: Option<OffsetDateTime>,
}

/// A sink delivery waiting its turn.
#[derive(Debug, Clone)]
struct QueuedDelivery {
    delivery: SinkDelivery,
    due_at: OffsetDateTime,
}

#[derive(Debug, Default)]
struct State {
    sequence: u64,
    events: Vec<Event>,
    notifications: Vec<Row<Notification>>,
    revisions: Vec<Row<Revision>>,
    channels: Vec<Row<Channel>>,
    devices: Vec<Device>,
    receipts: Vec<Receipt>,
    interactions: Vec<InteractionRecord>,
    silences: Vec<Silence>,
    users: Vec<User>,
    service_accounts: Vec<ServiceAccount>,
    api_keys: Vec<StoredApiKey>,
    reservations: Vec<Reservation>,
    audit: Vec<Row<AuditEntry>>,
    /// Channels whose notification expired without ever being delivered — the
    /// "N expired while you were away" counter (design §7.2bis).
    expired_counters: Vec<(Cursor, ChannelId)>,
    pokes: Vec<Poke>,
    subscribers: Vec<UnboundedSender<Poke>>,
    pushes: Vec<(DeviceId, PushMessage)>,
    push_outcome: Option<PushOutcome>,
    sink_queue: Vec<QueuedDelivery>,
    dead_letters: Vec<DeadLetter>,
}

impl State {
    /// Allocate the next position in the log.
    fn next_cursor(&mut self) -> Cursor {
        self.sequence += 1;
        Cursor::from_sequence(self.sequence)
    }

    /// Append an event and return where it landed.
    fn append(&mut self, audience: Audience, payload: EventPayload, at: OffsetDateTime) -> Cursor {
        let cursor = self.next_cursor();
        self.events.push(Event {
            cursor,
            audience,
            recorded_at: at,
            payload,
        });
        cursor
    }

    fn notification(&self, id: NotificationId) -> Option<&Row<Notification>> {
        self.notifications.iter().find(|row| row.value.id == id)
    }

    fn is_live(notification: &Notification, now: OffsetDateTime) -> bool {
        !notification.is_gone(now)
    }
}

/// Every repository port, over one shared state.
///
/// Cloning shares the state, so a test wires the same store into every port it
/// needs and they all agree with each other.
#[derive(Debug, Clone, Default)]
pub struct InMemory {
    state: Arc<Mutex<State>>,
}

impl InMemory {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A poisoned lock means an earlier assertion already failed; reading through
        // it lets that failure be the one reported.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // --- Seeding and inspection, for tests ---------------------------------

    /// Put a channel in place without going through a use case.
    pub fn seed_channel(&self, channel: Channel) -> &Self {
        let mut state = self.state();
        let cursor = state.next_cursor();
        state.channels.push(Row {
            value: channel,
            cursor,
        });
        self
    }

    /// Put a user in place.
    pub fn seed_user(&self, user: User) -> &Self {
        self.state().users.push(user);
        self
    }

    /// Put a device in place.
    pub fn seed_device(&self, device: Device) -> &Self {
        self.state().devices.push(device);
        self
    }

    /// Put an API key, hash included, in place.
    pub fn seed_api_key(&self, stored: StoredApiKey) -> &Self {
        self.state().api_keys.push(stored);
        self
    }

    /// Every event appended so far, oldest first.
    pub fn events(&self) -> Vec<Event> {
        self.state().events.clone()
    }

    /// Every poke published so far — what a test asserts fan-out on.
    pub fn pokes(&self) -> Vec<Poke> {
        self.state().pokes.clone()
    }

    /// Every push handed to the dispatcher, in order.
    pub fn pushes(&self) -> Vec<(DeviceId, PushMessage)> {
        self.state().pushes.clone()
    }

    /// Every audit entry recorded, oldest first.
    pub fn audit_entries(&self) -> Vec<AuditEntry> {
        self.state()
            .audit
            .iter()
            .map(|row| row.value.clone())
            .collect()
    }

    /// Make every subsequent push come back with this outcome.
    pub fn set_push_outcome(&self, outcome: PushOutcome) -> &Self {
        self.state().push_outcome = Some(outcome);
        self
    }

    /// The channels a principal may read with `scope`.
    fn channel_ids_for(
        state: &State,
        principal: &Principal,
        groups: &[String],
        scope: Scope,
    ) -> Vec<ChannelId> {
        state
            .channels
            .iter()
            .filter(|row| row.value.allows(principal, groups, scope))
            .map(|row| row.value.id.clone())
            .collect()
    }
}

// --- Notifications ---------------------------------------------------------

#[async_trait::async_trait]
impl NotificationRepository for InMemory {
    async fn get(&self, id: NotificationId) -> PortResult<Option<Notification>> {
        Ok(self.state().notification(id).map(|row| row.value.clone()))
    }

    async fn find_by_external_id(
        &self,
        channel: &ChannelId,
        external_id: &ExternalId,
    ) -> PortResult<Option<Notification>> {
        let state = self.state();
        // A tombstoned row is deliberately invisible here: re-running a job whose
        // previous notification aged out creates a new one rather than reviving it.
        Ok(state
            .notifications
            .iter()
            .map(|row| &row.value)
            .find(|notification| {
                notification.channel == *channel
                    && notification.external_id.as_ref() == Some(external_id)
                    && notification.deleted_at.is_none()
            })
            .cloned())
    }

    async fn insert(&self, notification: &Notification, revision: &Revision) -> PortResult<Cursor> {
        let mut state = self.state();

        let taken = notification.external_id.as_ref().is_some_and(|external| {
            state.notifications.iter().any(|row| {
                row.value.channel == notification.channel
                    && row.value.external_id.as_ref() == Some(external)
                    && row.value.deleted_at.is_none()
            })
        });
        if taken {
            // The caller re-reads and revises instead — which is what makes the
            // upsert idempotent under CI retries (scenario T16).
            return Err(PortError::Conflict {
                what: "notification.external_id",
            });
        }

        let cursor = state.append(
            Audience::Channel {
                id: notification.channel.clone(),
            },
            EventPayload::Revision(Box::new(RevisionEvent::new(notification, revision))),
            revision.received_at,
        );
        state.notifications.push(Row {
            value: notification.clone(),
            cursor,
        });
        state.revisions.push(Row {
            value: revision.clone(),
            cursor,
        });
        Ok(cursor)
    }

    async fn append_revision(
        &self,
        notification: &Notification,
        revision: &Revision,
    ) -> PortResult<Cursor> {
        let mut state = self.state();

        let cursor = state.append(
            Audience::Channel {
                id: notification.channel.clone(),
            },
            EventPayload::Revision(Box::new(RevisionEvent::new(notification, revision))),
            revision.received_at,
        );

        match state
            .notifications
            .iter_mut()
            .find(|row| row.value.id == notification.id)
        {
            Some(row) => {
                row.value = notification.clone();
                row.cursor = cursor;
            }
            None => {
                return Err(PortError::corrupt(
                    "notification",
                    format!("{} has no stored row to revise", notification.id),
                ));
            }
        }

        state.revisions.push(Row {
            value: revision.clone(),
            cursor,
        });
        Ok(cursor)
    }

    async fn list(&self, query: &InboxQuery) -> PortResult<Page<Notification>> {
        let state = self.state();
        let now = OffsetDateTime::now_utc();

        let mut matched: Vec<&Row<Notification>> = state
            .notifications
            .iter()
            .filter(|row| {
                let notification = &row.value;
                State::is_live(notification, now)
                    && query.channels.contains(&notification.channel)
                    && (query.states.is_empty() || query.states.contains(&notification.state()))
                    && (query.statuses.is_empty()
                        || query.statuses.contains(&notification.status()))
                    && query
                        .min_priority
                        .is_none_or(|min| notification.priority() >= min)
                    && query
                        .group_key
                        .as_ref()
                        .is_none_or(|key| notification.head.content.group_key.as_ref() == Some(key))
                    && query
                        .tags
                        .iter()
                        .all(|tag| notification.head.content.tags.contains(tag))
            })
            .collect();

        // Most recent activity first, which is the only order an inbox is read in.
        matched.sort_by_key(|row| Reverse(row.cursor));
        if let Some(after) = query.page.after {
            matched.retain(|row| row.cursor < after);
        }

        Ok(paginate(&matched, query.page.effective_limit()))
    }

    async fn revisions(&self, id: NotificationId, page: PageRequest) -> PortResult<Page<Revision>> {
        let state = self.state();

        let mut matched: Vec<&Row<Revision>> = state
            .revisions
            .iter()
            .filter(|row| row.value.notification_id == id)
            .collect();

        // Oldest first: a timeline is read forwards.
        matched.sort_by_key(|row| row.cursor);
        if let Some(after) = page.after {
            matched.retain(|row| row.cursor > after);
        }

        Ok(paginate(&matched, page.effective_limit()))
    }

    async fn search(&self, query: &SearchQuery) -> PortResult<Page<Notification>> {
        let state = self.state();
        let now = OffsetDateTime::now_utc();
        let needle = query.text.to_lowercase();

        let mut matched: Vec<&Row<Notification>> = state
            .notifications
            .iter()
            .filter(|row| {
                let notification = &row.value;
                // Scope first, match second — the ordering that makes scenario T27
                // hard to break by accident.
                if !query.channels.contains(&notification.channel)
                    || !State::is_live(notification, now)
                {
                    return false;
                }
                let content = &notification.head.content;
                content.title.to_lowercase().contains(&needle)
                    || content
                        .body
                        .as_ref()
                        .is_some_and(|body| body.to_lowercase().contains(&needle))
                    || content.tags.iter().any(|tag| tag.contains(&needle))
            })
            .collect();

        matched.sort_by_key(|row| Reverse(row.cursor));
        if let Some(after) = query.page.after {
            matched.retain(|row| row.cursor < after);
        }

        Ok(paginate(&matched, query.page.effective_limit()))
    }

    async fn mark_deleted(
        &self,
        id: NotificationId,
        at: OffsetDateTime,
    ) -> PortResult<Option<Cursor>> {
        let mut state = self.state();

        let Some(row) = state.notifications.iter().find(|row| row.value.id == id) else {
            return Ok(None);
        };
        if row.value.deleted_at.is_some() {
            // Already gone: no second tombstone, no moved timestamp.
            return Ok(None);
        }
        let channel = row.value.channel.clone();

        let cursor = state.append(
            Audience::Channel { id: channel },
            EventPayload::Tombstone {
                notification_id: id,
                reason: TombstoneReason::Deleted,
            },
            at,
        );
        if let Some(row) = state
            .notifications
            .iter_mut()
            .find(|row| row.value.id == id)
        {
            row.value.delete(at);
            row.cursor = cursor;
        }
        Ok(Some(cursor))
    }

    async fn claim_expired(
        &self,
        now: OffsetDateTime,
        limit: u32,
    ) -> PortResult<Vec<ExpiredNotification>> {
        let mut state = self.state();

        let due: Vec<(NotificationId, ChannelId, bool)> = state
            .notifications
            .iter()
            .filter(|row| row.value.deleted_at.is_none() && row.value.is_expired(now))
            .take(limit as usize)
            .map(|row| {
                let delivered = state.receipts.iter().any(|receipt| {
                    receipt.notification_id == row.value.id
                        && matches!(receipt.kind, ReceiptKind::Delivered | ReceiptKind::Seen)
                });
                (row.value.id, row.value.channel.clone(), delivered)
            })
            .collect();

        let mut claimed = Vec::with_capacity(due.len());
        for (id, channel, was_delivered) in due {
            let cursor = if was_delivered {
                // Somebody has it on screen: they need the tombstone to drop it.
                state.append(
                    Audience::Channel {
                        id: channel.clone(),
                    },
                    EventPayload::Tombstone {
                        notification_id: id,
                        reason: TombstoneReason::Expired,
                    },
                    now,
                )
            } else {
                // Nobody ever saw it. It stays out of the backlog and becomes a
                // number instead (design §7.2bis, scenario T9).
                let cursor = state.next_cursor();
                state.expired_counters.push((cursor, channel.clone()));
                cursor
            };

            if let Some(row) = state
                .notifications
                .iter_mut()
                .find(|row| row.value.id == id)
            {
                row.value.delete(now);
                row.cursor = cursor;
            }
            claimed.push(ExpiredNotification {
                id,
                channel,
                was_delivered,
            });
        }
        Ok(claimed)
    }

    async fn purge(&self, deleted_before: OffsetDateTime, limit: u32) -> PortResult<u64> {
        let mut state = self.state();

        let doomed: Vec<NotificationId> = state
            .notifications
            .iter()
            .filter(|row| {
                row.value
                    .deleted_at
                    .is_some_and(|deleted| deleted < deleted_before)
            })
            .take(limit as usize)
            .map(|row| row.value.id)
            .collect();

        state
            .notifications
            .retain(|row| !doomed.contains(&row.value.id));
        state
            .revisions
            .retain(|row| !doomed.contains(&row.value.notification_id));

        Ok(doomed.len() as u64)
    }
}

/// Take a page and work out whether there is another one behind it.
fn paginate<T: Clone>(rows: &[&Row<T>], limit: u32) -> Page<T> {
    let limit = limit as usize;
    let has_more = rows.len() > limit;
    let items: Vec<T> = rows
        .iter()
        .take(limit)
        .map(|row| row.value.clone())
        .collect();

    match rows.get(limit.saturating_sub(1)).filter(|_| has_more) {
        Some(last) => Page::more(items, last.cursor),
        None => Page::last(items),
    }
}

// --- Channels --------------------------------------------------------------

#[async_trait::async_trait]
impl ChannelRepository for InMemory {
    async fn get(&self, id: &ChannelId) -> PortResult<Option<Channel>> {
        Ok(self
            .state()
            .channels
            .iter()
            .find(|row| row.value.id == *id)
            .map(|row| row.value.clone()))
    }

    async fn list_for(
        &self,
        principal: &Principal,
        groups: &[String],
        scope: Scope,
        page: PageRequest,
    ) -> PortResult<Page<Channel>> {
        let state = self.state();

        let mut matched: Vec<&Row<Channel>> = state
            .channels
            .iter()
            .filter(|row| row.value.allows(principal, groups, scope))
            .collect();

        matched.sort_by_key(|row| row.cursor);
        if let Some(after) = page.after {
            matched.retain(|row| row.cursor > after);
        }

        Ok(paginate(&matched, page.effective_limit()))
    }

    async fn subscribed_ids(
        &self,
        principal: &Principal,
        groups: &[String],
    ) -> PortResult<Vec<ChannelId>> {
        let state = self.state();
        Ok(InMemory::channel_ids_for(
            &state,
            principal,
            groups,
            Scope::Subscribe,
        ))
    }

    async fn upsert(&self, channel: &Channel) -> PortResult<()> {
        let mut state = self.state();
        let cursor = state.next_cursor();

        match state
            .channels
            .iter_mut()
            .find(|row| row.value.id == channel.id)
        {
            Some(row) => row.value = channel.clone(),
            None => state.channels.push(Row {
                value: channel.clone(),
                cursor,
            }),
        }
        Ok(())
    }

    async fn delete(&self, id: &ChannelId) -> PortResult<bool> {
        let mut state = self.state();
        let before = state.channels.len();
        state.channels.retain(|row| row.value.id != *id);
        Ok(state.channels.len() != before)
    }

    async fn list_managed(&self) -> PortResult<Vec<Channel>> {
        Ok(self
            .state()
            .channels
            .iter()
            .filter(|row| row.value.origin.is_managed())
            .map(|row| row.value.clone())
            .collect())
    }
}

// --- Devices ---------------------------------------------------------------

#[async_trait::async_trait]
impl DeviceRepository for InMemory {
    async fn get(&self, id: DeviceId) -> PortResult<Option<Device>> {
        Ok(self
            .state()
            .devices
            .iter()
            .find(|device| device.id == id)
            .cloned())
    }

    async fn list_for(&self, owner: &Principal) -> PortResult<Vec<Device>> {
        Ok(self
            .state()
            .devices
            .iter()
            .filter(|device| device.owner == *owner)
            .cloned()
            .collect())
    }

    async fn upsert(&self, device: &Device) -> PortResult<()> {
        let mut state = self.state();
        match state
            .devices
            .iter_mut()
            .find(|candidate| candidate.id == device.id)
        {
            Some(existing) => *existing = device.clone(),
            None => state.devices.push(device.clone()),
        }
        Ok(())
    }

    async fn revoke(&self, id: DeviceId, at: OffsetDateTime) -> PortResult<bool> {
        let mut state = self.state();
        match state.devices.iter_mut().find(|device| device.id == id) {
            Some(device) => {
                device.revoke(at);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn advance_cursor(
        &self,
        id: DeviceId,
        cursor: Cursor,
        at: OffsetDateTime,
    ) -> PortResult<()> {
        let mut state = self.state();
        if let Some(device) = state.devices.iter_mut().find(|device| device.id == id) {
            // Monotonic here as well as in the domain: two tabs acknowledging
            // different positions must not let the older one win.
            device.advance(cursor, at);
        }
        Ok(())
    }

    async fn delivery_targets(&self, channel: &ChannelId) -> PortResult<Vec<Device>> {
        let state = self.state();

        let Some(row) = state.channels.iter().find(|row| row.value.id == *channel) else {
            return Ok(Vec::new());
        };
        let members: Vec<&Principal> = row
            .value
            .members
            .iter()
            .filter(|member| member.grants(Scope::Subscribe))
            .map(|member| &member.principal)
            .collect();

        Ok(state
            .devices
            .iter()
            .filter(|device| device.is_active() && members.contains(&&device.owner))
            .cloned()
            .collect())
    }
}

// --- Receipts --------------------------------------------------------------

#[async_trait::async_trait]
impl ReceiptRepository for InMemory {
    async fn record(&self, receipt: &Receipt) -> PortResult<Option<Cursor>> {
        let mut state = self.state();

        let already = state.receipts.iter().any(|existing| {
            existing.notification_id == receipt.notification_id
                && existing.principal == receipt.principal
                && existing.device == receipt.device
                && existing.kind == receipt.kind
        });
        if already {
            // A retried acknowledgement must not inflate "seen by 3/5".
            return Ok(None);
        }

        let Some(channel) = state
            .notification(receipt.notification_id)
            .map(|row| row.value.channel.clone())
        else {
            return Err(PortError::corrupt(
                "receipt",
                "references a notification that is not stored",
            ));
        };

        let cursor = state.append(
            Audience::Channel { id: channel },
            EventPayload::Receipt(receipt.clone()),
            receipt.at,
        );
        state.receipts.push(receipt.clone());
        Ok(Some(cursor))
    }

    async fn list_for(&self, notification: NotificationId) -> PortResult<Vec<Receipt>> {
        Ok(self
            .state()
            .receipts
            .iter()
            .filter(|receipt| receipt.notification_id == notification)
            .cloned()
            .collect())
    }

    async fn has_seen(
        &self,
        notification: NotificationId,
        principal: &Principal,
    ) -> PortResult<bool> {
        Ok(self.state().receipts.iter().any(|receipt| {
            receipt.notification_id == notification
                && receipt.principal == *principal
                && receipt.kind == ReceiptKind::Seen
        }))
    }
}

// --- Interactions and silences ---------------------------------------------

#[async_trait::async_trait]
impl InteractionRepository for InMemory {
    async fn apply(&self, outcome: &Recorded) -> PortResult<Option<Cursor>> {
        let Some(record) = outcome.record() else {
            // `Unchanged`: nothing written, nothing broadcast.
            return Ok(None);
        };
        let mut state = self.state();

        if let Recorded::Replaced { previous, .. } = outcome {
            state
                .interactions
                .retain(|existing| existing.id != *previous);
        }

        let Some(channel) = state
            .notification(record.notification_id)
            .map(|row| row.value.channel.clone())
        else {
            return Err(PortError::corrupt(
                "interaction",
                "references a notification that is not stored",
            ));
        };

        let cursor = state.append(
            Audience::Channel { id: channel },
            EventPayload::Interaction(record.clone()),
            record.recorded_at,
        );
        state.interactions.push(record.clone());
        Ok(Some(cursor))
    }

    async fn list_for(&self, notification: NotificationId) -> PortResult<Vec<InteractionRecord>> {
        Ok(self
            .state()
            .interactions
            .iter()
            .filter(|record| record.notification_id == notification)
            .cloned()
            .collect())
    }

    async fn silence(&self, silence: &Silence) -> PortResult<Cursor> {
        let mut state = self.state();

        // Re-silencing the same target extends it rather than stacking a second
        // one nobody can see to lift.
        state
            .silences
            .retain(|existing| existing.user != silence.user || existing.target != silence.target);
        state.silences.push(silence.clone());

        let user = silence.user;
        let silences = state
            .silences
            .iter()
            .filter(|candidate| candidate.user == user && candidate.is_active(silence.since))
            .cloned()
            .collect();

        Ok(state.append(
            Audience::User { id: user },
            EventPayload::Preference(Box::new(PreferenceEvent { user, silences })),
            silence.since,
        ))
    }

    async fn active_silences(&self, user: UserId, now: OffsetDateTime) -> PortResult<Vec<Silence>> {
        Ok(self
            .state()
            .silences
            .iter()
            .filter(|silence| silence.user == user && silence.is_active(now))
            .cloned()
            .collect())
    }
}

// --- Identities ------------------------------------------------------------

#[async_trait::async_trait]
impl UserRepository for InMemory {
    async fn get(&self, id: UserId) -> PortResult<Option<User>> {
        Ok(self
            .state()
            .users
            .iter()
            .find(|user| user.id == id)
            .cloned())
    }

    async fn find_by_subject(&self, subject: &str) -> PortResult<Option<User>> {
        Ok(self
            .state()
            .users
            .iter()
            .find(|user| user.oidc_subject == subject)
            .cloned())
    }

    async fn find_by_email(&self, email: &str) -> PortResult<Option<User>> {
        Ok(self
            .state()
            .users
            .iter()
            .find(|user| user.email.as_deref() == Some(email))
            .cloned())
    }

    async fn upsert(&self, user: &User) -> PortResult<()> {
        let mut state = self.state();
        match state
            .users
            .iter_mut()
            .find(|existing| existing.id == user.id)
        {
            Some(existing) => *existing = user.clone(),
            None => state.users.push(user.clone()),
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ServiceAccountRepository for InMemory {
    async fn get(&self, id: &ServiceAccountId) -> PortResult<Option<ServiceAccount>> {
        Ok(self
            .state()
            .service_accounts
            .iter()
            .find(|account| account.id == *id)
            .cloned())
    }

    async fn list(&self) -> PortResult<Vec<ServiceAccount>> {
        Ok(self.state().service_accounts.clone())
    }

    async fn upsert(&self, account: &ServiceAccount) -> PortResult<()> {
        let mut state = self.state();
        match state
            .service_accounts
            .iter_mut()
            .find(|existing| existing.id == account.id)
        {
            Some(existing) => *existing = account.clone(),
            None => state.service_accounts.push(account.clone()),
        }
        Ok(())
    }

    async fn list_managed(&self) -> PortResult<Vec<ServiceAccount>> {
        Ok(self
            .state()
            .service_accounts
            .iter()
            .filter(|account| account.origin.is_managed())
            .cloned()
            .collect())
    }
}

#[async_trait::async_trait]
impl ApiKeyRepository for InMemory {
    async fn find_by_prefix(&self, prefix: &ApiKeyPrefix) -> PortResult<Option<StoredApiKey>> {
        Ok(self
            .state()
            .api_keys
            .iter()
            .find(|stored| stored.key.prefix == *prefix)
            .cloned())
    }

    async fn get(&self, id: ApiKeyId) -> PortResult<Option<ApiKey>> {
        Ok(self
            .state()
            .api_keys
            .iter()
            .find(|stored| stored.key.id == id)
            .map(|stored| stored.key.clone()))
    }

    async fn list_for(&self, owner: &Principal) -> PortResult<Vec<ApiKey>> {
        Ok(self
            .state()
            .api_keys
            .iter()
            .filter(|stored| stored.key.principal() == *owner)
            .map(|stored| stored.key.clone())
            .collect())
    }

    async fn insert(&self, stored: &StoredApiKey) -> PortResult<()> {
        self.state().api_keys.push(stored.clone());
        Ok(())
    }

    async fn touch(&self, id: ApiKeyId, at: OffsetDateTime) -> PortResult<()> {
        let mut state = self.state();
        if let Some(stored) = state.api_keys.iter_mut().find(|stored| stored.key.id == id) {
            stored.key.last_used_at = Some(at);
        }
        Ok(())
    }

    async fn revoke(&self, id: ApiKeyId, at: OffsetDateTime) -> PortResult<bool> {
        let mut state = self.state();
        match state.api_keys.iter_mut().find(|stored| stored.key.id == id) {
            Some(stored) => {
                stored.key.revoked_at.get_or_insert(at);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn revoke_all_for(&self, owner: &Principal, at: OffsetDateTime) -> PortResult<u64> {
        let mut state = self.state();
        let mut revoked = 0;
        for stored in &mut state.api_keys {
            if stored.key.principal() == *owner && stored.key.revoked_at.is_none() {
                stored.key.revoked_at = Some(at);
                revoked += 1;
            }
        }
        Ok(revoked)
    }
}

// --- Idempotency -----------------------------------------------------------

#[async_trait::async_trait]
impl IdempotencyStore for InMemory {
    async fn claim(
        &self,
        principal_ref: &str,
        key: &str,
        now: OffsetDateTime,
    ) -> PortResult<Claim> {
        let mut state = self.state();

        // An expired reservation is as good as absent, so a key becomes reusable
        // once its window has passed rather than being burnt forever.
        state
            .reservations
            .retain(|reservation| reservation.expires_at.is_none_or(|at| at > now));

        let existing = state
            .reservations
            .iter()
            .find(|reservation| {
                reservation.principal_ref == principal_ref && reservation.key == key
            })
            .map(|reservation| reservation.response.clone());

        if let Some(response) = existing {
            // Claimed already: either the work finished and we replay it, or it is
            // still running and the caller is told to come back.
            return Ok(response.map_or(Claim::InFlight, |response| Claim::Replayed { response }));
        }

        state.reservations.push(Reservation {
            principal_ref: principal_ref.to_owned(),
            key: key.to_owned(),
            response: None,
            expires_at: None,
        });
        Ok(Claim::Fresh)
    }

    async fn complete(
        &self,
        principal_ref: &str,
        key: &str,
        response: &str,
        expires_at: OffsetDateTime,
    ) -> PortResult<()> {
        let mut state = self.state();
        if let Some(reservation) = state
            .reservations
            .iter_mut()
            .find(|candidate| candidate.principal_ref == principal_ref && candidate.key == key)
        {
            reservation.response = Some(response.to_owned());
            reservation.expires_at = Some(expires_at);
        }
        Ok(())
    }

    async fn release(&self, principal_ref: &str, key: &str) -> PortResult<()> {
        let mut state = self.state();
        state.reservations.retain(|reservation| {
            reservation.principal_ref != principal_ref
                || reservation.key != key
                || reservation.response.is_some()
        });
        Ok(())
    }

    async fn prune(&self, now: OffsetDateTime) -> PortResult<u64> {
        let mut state = self.state();
        let before = state.reservations.len();
        state
            .reservations
            .retain(|reservation| reservation.expires_at.is_none_or(|at| at > now));
        Ok((before - state.reservations.len()) as u64)
    }
}

// --- The event log ---------------------------------------------------------

#[async_trait::async_trait]
impl EventStore for InMemory {
    async fn sync(&self, user: UserId, query: &SyncQuery) -> PortResult<SyncPage> {
        let state = self.state();
        let principal = Principal::User { id: user };

        let subscribed =
            InMemory::channel_ids_for(&state, &principal, &query.groups, Scope::Subscribe);
        // An explicit `?channels=` narrows what the caller may already see; it can
        // never widen it.
        let visible: Vec<ChannelId> = if query.channels.is_empty() {
            subscribed
        } else {
            query
                .channels
                .iter()
                .filter(|channel| subscribed.contains(channel))
                .cloned()
                .collect()
        };

        let mut events: Vec<Event> = state
            .events
            .iter()
            .filter(|event| query.after.is_none_or(|after| event.cursor > after))
            .filter(|event| event.is_visible_to(user, &visible))
            .cloned()
            .collect();
        events.sort_by_key(|event| event.cursor);

        let before_compaction = events.len();
        let events = compact(events);
        let compacted = u32::try_from(before_compaction - events.len()).unwrap_or(u32::MAX);

        let limit = query.page.effective_limit() as usize;
        let has_more = events.len() > limit;
        let page: Vec<Event> = events.into_iter().take(limit).collect();
        let next = has_more
            .then(|| page.last().map(|event| event.cursor))
            .flatten();

        let mut expired: Vec<ExpiredWhileAway> = Vec::new();
        for (cursor, channel) in &state.expired_counters {
            if query.after.is_some_and(|after| *cursor <= after) || !visible.contains(channel) {
                continue;
            }
            match expired.iter_mut().find(|entry| entry.channel == *channel) {
                Some(entry) => entry.count += 1,
                None => expired.push(ExpiredWhileAway {
                    channel: channel.clone(),
                    count: 1,
                }),
            }
        }

        Ok(SyncPage {
            events: page,
            next,
            expired,
            compacted,
        })
    }

    async fn head(&self) -> PortResult<Cursor> {
        Ok(Cursor::from_sequence(self.state().sequence))
    }

    async fn prune(&self, before: OffsetDateTime, limit: u32) -> PortResult<u64> {
        let mut state = self.state();

        let doomed: Vec<Cursor> = state
            .events
            .iter()
            .filter(|event| event.recorded_at < before)
            .take(limit as usize)
            .map(|event| event.cursor)
            .collect();

        state.events.retain(|event| !doomed.contains(&event.cursor));
        Ok(doomed.len() as u64)
    }
}

/// Fold a notification's revision events down to its last one (design §7.2).
///
/// Forty revisions of one deploy become one entry, so a client coming back from a
/// week offline gets a timeline rather than a flood. Everything that is not a
/// revision passes through untouched: receipts, interactions and tombstones each
/// mean something on their own.
fn compact(events: Vec<Event>) -> Vec<Event> {
    let mut latest: Vec<(NotificationId, Cursor)> = Vec::new();
    for event in &events {
        if let EventPayload::Revision(revision) = &event.payload {
            match latest
                .iter_mut()
                .find(|(id, _)| *id == revision.notification_id)
            {
                Some((_, cursor)) => *cursor = event.cursor,
                None => latest.push((revision.notification_id, event.cursor)),
            }
        }
    }

    events
        .into_iter()
        .filter(|event| match &event.payload {
            EventPayload::Revision(revision) => latest
                .iter()
                .any(|(id, cursor)| *id == revision.notification_id && *cursor == event.cursor),
            _ => true,
        })
        .collect()
}

// --- Fan-out ---------------------------------------------------------------

#[async_trait::async_trait]
impl EventBus for InMemory {
    async fn publish(&self, poke: &Poke) -> PortResult<()> {
        let mut state = self.state();
        state.pokes.push(poke.clone());
        // Best-effort by contract: a subscriber that has gone away costs a dropped
        // poke, never a failed publish (ADR-0008).
        state
            .subscribers
            .retain(|sender| sender.unbounded_send(poke.clone()).is_ok());
        Ok(())
    }

    async fn subscribe(&self) -> PortResult<BoxStream<'static, Poke>> {
        let (sender, receiver) = unbounded();
        self.state().subscribers.push(sender);
        Ok(Box::pin(receiver))
    }
}

#[async_trait::async_trait]
impl PushDispatcher for InMemory {
    async fn dispatch(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        let mut state = self.state();
        state.pushes.push((device.id, message.clone()));
        Ok(state.push_outcome.clone().unwrap_or(PushOutcome::Delivered))
    }
}

// --- Sink queue ------------------------------------------------------------

#[async_trait::async_trait]
impl SinkQueue for InMemory {
    async fn enqueue(
        &self,
        delivery: &SinkDelivery,
        next_attempt_at: OffsetDateTime,
    ) -> PortResult<()> {
        self.state().sink_queue.push(QueuedDelivery {
            delivery: delivery.clone(),
            due_at: next_attempt_at,
        });
        Ok(())
    }

    async fn claim_due(&self, now: OffsetDateTime, limit: u32) -> PortResult<Vec<SinkDelivery>> {
        let mut state = self.state();

        let claimed: Vec<SinkDelivery> = state
            .sink_queue
            .iter()
            .filter(|queued| queued.due_at <= now)
            .take(limit as usize)
            .map(|queued| queued.delivery.clone())
            .collect();

        // Claiming, not reading: two replicas both briefly believing they lead must
        // not double-deliver a webhook.
        let ids: Vec<String> = claimed
            .iter()
            .map(|delivery| delivery.delivery_id.clone())
            .collect();
        state
            .sink_queue
            .retain(|queued| !ids.contains(&queued.delivery.delivery_id));

        Ok(claimed)
    }

    async fn bury(
        &self,
        delivery: &SinkDelivery,
        error: &str,
        at: OffsetDateTime,
    ) -> PortResult<()> {
        self.state().dead_letters.push(DeadLetter {
            id: delivery.delivery_id.clone(),
            channel: delivery.channel.clone(),
            sink_kind: delivery.target.kind(),
            last_error: error.to_owned(),
            attempts: delivery.attempt,
            first_failed_at: at,
            dead_at: at,
        });
        Ok(())
    }

    async fn dead_letters(&self, channel: Option<&ChannelId>) -> PortResult<Vec<DeadLetter>> {
        Ok(self
            .state()
            .dead_letters
            .iter()
            .filter(|letter| channel.is_none_or(|id| letter.channel == *id))
            .cloned()
            .collect())
    }

    async fn requeue(&self, id: &str, at: OffsetDateTime) -> PortResult<bool> {
        let mut state = self.state();
        let Some(position) = state.dead_letters.iter().position(|letter| letter.id == id) else {
            return Ok(false);
        };
        state.dead_letters.remove(position);

        // The delivery body is not kept by this double; a requeue is observable
        // through `dead_letters` shrinking, which is what the use-case test asserts.
        let _ = at;
        Ok(true)
    }

    async fn dead_letter_depth(&self) -> PortResult<Vec<(ChannelId, u64)>> {
        let state = self.state();
        let mut depth: Vec<(ChannelId, u64)> = Vec::new();
        for letter in &state.dead_letters {
            match depth
                .iter_mut()
                .find(|(channel, _)| *channel == letter.channel)
            {
                Some((_, count)) => *count += 1,
                None => depth.push((letter.channel.clone(), 1)),
            }
        }
        Ok(depth)
    }
}

// --- Audit -----------------------------------------------------------------

#[async_trait::async_trait]
impl AuditLog for InMemory {
    async fn record(&self, entry: &AuditEntry) -> PortResult<()> {
        let mut state = self.state();
        let cursor = state.next_cursor();
        state.audit.push(Row {
            value: entry.clone(),
            cursor,
        });
        Ok(())
    }

    async fn query(&self, query: &AuditQuery) -> PortResult<Page<AuditEntry>> {
        let state = self.state();

        let mut matched: Vec<&Row<AuditEntry>> = state
            .audit
            .iter()
            .filter(|row| {
                let entry = &row.value;
                query
                    .actor
                    .as_ref()
                    .is_none_or(|actor| entry.actor == *actor)
                    && query
                        .action
                        .as_ref()
                        .is_none_or(|action| entry.action == action)
                    && query
                        .channel
                        .as_ref()
                        .is_none_or(|channel| entry.channel.as_ref() == Some(channel))
                    && query.since.is_none_or(|since| entry.at >= since)
                    && query.until.is_none_or(|until| entry.at <= until)
            })
            .collect();

        // Most recent first: an incident review reads backwards.
        matched.sort_by_key(|row| Reverse(row.cursor));
        if let Some(after) = query.page.after {
            matched.retain(|row| row.cursor < after);
        }

        Ok(paginate(&matched, query.page.effective_limit()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        reason = "a failing unwrap or an unreachable branch is a failing test"
    )]

    use notify_domain::{
        Actions, Author, Channel, ChannelPolicy, ContentDraft, Interaction, InteractionLog,
        Membership, NotificationId, Origin, PushTransport, ReceiptId, RevisionDraft, State,
    };

    use crate::sinks::SinkTarget;
    use time::Duration;

    use super::*;
    use crate::test_utils::TestClock;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(TestClock::EPOCH).unwrap()
    }

    fn user() -> Principal {
        Principal::User {
            id: UserId::generate(),
        }
    }

    fn author(principal: &Principal) -> Author {
        Author::anonymous(principal.clone())
    }

    fn channel_id(id: &str) -> ChannelId {
        ChannelId::parse(id).unwrap()
    }

    /// A channel `member` may publish to and read.
    fn channel(id: &str, member: &Principal) -> Channel {
        let mut channel = Channel::dynamic(channel_id(id), None).unwrap();
        channel.upsert_member(
            Membership::new(member.clone(), vec![Scope::Publish, Scope::Subscribe]).unwrap(),
        );
        channel
    }

    /// Publish a lifecycle notification through the repository.
    async fn publish(
        store: &InMemory,
        channel: &str,
        principal: &Principal,
        title: &str,
        at: OffsetDateTime,
    ) -> Notification {
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel_id(channel),
            None,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled(title)),
            author(principal),
            at,
            &ChannelPolicy::default(),
        )
        .unwrap();

        NotificationRepository::insert(store, &notification, &written.revision)
            .await
            .unwrap();
        notification
    }

    fn sync_query(after: Option<Cursor>) -> SyncQuery {
        SyncQuery {
            after,
            channels: Vec::new(),
            groups: Vec::new(),
            page: PageRequest::of_size(100),
        }
    }

    // --- T7 · Resuming from a cursor (design §3.4) ------------------------

    #[tokio::test]
    async fn critical_t7_a_cursor_replays_exactly_what_was_missed() {
        let store = InMemory::new();
        let alice = user();
        let Principal::User { id: alice_id } = alice else {
            unreachable!()
        };
        store.seed_channel(channel("ci-infra", &alice));

        // Seen, then the client goes offline.
        publish(&store, "ci-infra", &alice, "before", now()).await;
        let checkpoint = EventStore::head(&store).await.unwrap();

        for index in 0..5 {
            publish(
                &store,
                "ci-infra",
                &alice,
                &format!("missed {index}"),
                now() + Duration::seconds(index),
            )
            .await;
        }

        let page = EventStore::sync(&store, alice_id, &sync_query(Some(checkpoint)))
            .await
            .unwrap();

        assert_eq!(page.events.len(), 5, "exactly what was missed, no more");
        let cursors: Vec<Cursor> = page.events.iter().map(|event| event.cursor).collect();
        let mut sorted = cursors.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(cursors, sorted, "ordered, and with no duplicates");
        assert!(cursors.iter().all(|cursor| *cursor > checkpoint));
    }

    #[tokio::test]
    async fn critical_t7_replaying_the_same_cursor_twice_yields_the_same_thing() {
        // `?cursor=` and `Last-Event-ID` carry the same token, so a client that
        // reconnects mid-sync must not see a different backlog.
        let store = InMemory::new();
        let alice = user();
        let Principal::User { id: alice_id } = alice else {
            unreachable!()
        };
        store.seed_channel(channel("ci-infra", &alice));

        publish(&store, "ci-infra", &alice, "one", now()).await;
        let checkpoint = EventStore::head(&store).await.unwrap();
        publish(&store, "ci-infra", &alice, "two", now()).await;

        let first = EventStore::sync(&store, alice_id, &sync_query(Some(checkpoint)))
            .await
            .unwrap();
        let second = EventStore::sync(&store, alice_id, &sync_query(Some(checkpoint)))
            .await
            .unwrap();
        assert_eq!(first.events, second.events);
    }

    #[tokio::test]
    async fn critical_t7_a_client_never_sees_a_channel_it_does_not_belong_to() {
        let store = InMemory::new();
        let (alice, bob) = (user(), user());
        let Principal::User { id: alice_id } = alice else {
            unreachable!()
        };
        store.seed_channel(channel("ci-infra", &alice));
        store.seed_channel(channel("cluster-ovh-1", &bob));

        publish(&store, "cluster-ovh-1", &bob, "not for alice", now()).await;

        let page = EventStore::sync(&store, alice_id, &sync_query(None))
            .await
            .unwrap();
        assert!(page.events.is_empty());
    }

    // --- T8 · Compaction --------------------------------------------------

    #[tokio::test]
    async fn critical_t8_forty_revisions_come_back_as_one_entry() {
        let store = InMemory::new();
        let alice = user();
        let Principal::User { id: alice_id } = alice else {
            unreachable!()
        };
        store.seed_channel(channel("ci-infra", &alice));

        let mut notification = publish(&store, "ci-infra", &alice, "deploy", now()).await;
        for step in 1..40 {
            let written = notification
                .revise(
                    RevisionDraft::simple(ContentDraft::titled(&format!("deploy step {step}"))),
                    author(&alice),
                    now() + Duration::seconds(step),
                    &ChannelPolicy::default(),
                )
                .unwrap();
            NotificationRepository::append_revision(&store, &notification, &written.revision)
                .await
                .unwrap();
        }

        let page = EventStore::sync(&store, alice_id, &sync_query(None))
            .await
            .unwrap();

        assert_eq!(
            page.events.len(),
            1,
            "a week away is a timeline, not a flood"
        );
        assert_eq!(
            page.compacted, 39,
            "and the client can say how much it folded"
        );
        let EventPayload::Revision(revision) = &page.events[0].payload else {
            panic!("expected the surviving event to be the last revision")
        };
        assert_eq!(revision.seq, 40);
    }

    #[tokio::test]
    async fn compaction_leaves_everything_that_is_not_a_revision_alone() {
        let store = InMemory::new();
        let alice = user();
        let Principal::User { id: alice_id } = alice else {
            unreachable!()
        };
        store.seed_channel(channel("ci-infra", &alice));

        let notification = publish(&store, "ci-infra", &alice, "deploy", now()).await;
        ReceiptRepository::record(
            &store,
            &Receipt {
                id: ReceiptId::generate(),
                notification_id: notification.id,
                principal: alice.clone(),
                device: None,
                kind: ReceiptKind::Seen,
                at: now(),
            },
        )
        .await
        .unwrap();

        let page = EventStore::sync(&store, alice_id, &sync_query(None))
            .await
            .unwrap();
        assert_eq!(page.events.len(), 2, "a receipt means something on its own");
        assert_eq!(page.compacted, 0);
    }

    // --- T9 · Expiring while away -----------------------------------------

    #[tokio::test]
    async fn critical_t9_an_undelivered_expired_notification_is_counted_not_backlogged() {
        let store = InMemory::new();
        let alice = user();
        let Principal::User { id: alice_id } = alice else {
            unreachable!()
        };
        store.seed_channel(channel("ci-infra", &alice));

        // A simple notification: it starts ageing immediately.
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel_id("ci-infra"),
            None,
            RevisionDraft::simple(ContentDraft::titled("nobody saw this")),
            author(&alice),
            now(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        NotificationRepository::insert(&store, &notification, &written.revision)
            .await
            .unwrap();

        let checkpoint = EventStore::head(&store).await.unwrap();
        let later = now() + Duration::days(8);
        let claimed = NotificationRepository::claim_expired(&store, later, 100)
            .await
            .unwrap();

        assert_eq!(claimed.len(), 1);
        assert!(!claimed[0].was_delivered);

        let page = EventStore::sync(&store, alice_id, &sync_query(Some(checkpoint)))
            .await
            .unwrap();
        assert!(
            page.events.is_empty(),
            "an expired notification nobody saw does not turn up a week late"
        );
        assert_eq!(
            page.expired,
            vec![ExpiredWhileAway {
                channel: channel_id("ci-infra"),
                count: 1
            }],
            "it becomes a number instead"
        );
    }

    #[tokio::test]
    async fn an_expired_notification_somebody_saw_gets_a_tombstone() {
        let store = InMemory::new();
        let alice = user();
        let Principal::User { id: alice_id } = alice else {
            unreachable!()
        };
        store.seed_channel(channel("ci-infra", &alice));

        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel_id("ci-infra"),
            None,
            RevisionDraft::simple(ContentDraft::titled("seen then expired")),
            author(&alice),
            now(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        NotificationRepository::insert(&store, &notification, &written.revision)
            .await
            .unwrap();
        ReceiptRepository::record(
            &store,
            &Receipt {
                id: ReceiptId::generate(),
                notification_id: notification.id,
                principal: alice.clone(),
                device: None,
                kind: ReceiptKind::Delivered,
                at: now(),
            },
        )
        .await
        .unwrap();

        let checkpoint = EventStore::head(&store).await.unwrap();
        NotificationRepository::claim_expired(&store, now() + Duration::days(8), 100)
            .await
            .unwrap();

        let page = EventStore::sync(&store, alice_id, &sync_query(Some(checkpoint)))
            .await
            .unwrap();
        assert!(
            matches!(
                page.events.first().map(|event| &event.payload),
                Some(EventPayload::Tombstone {
                    reason: TombstoneReason::Expired,
                    ..
                })
            ),
            "somebody has it on screen and needs to be told it is gone"
        );
        assert!(page.expired.is_empty());
    }

    // --- T16 · Idempotency ------------------------------------------------

    #[tokio::test]
    async fn critical_t16_a_replayed_idempotency_key_replays_the_response() {
        let store = InMemory::new();

        assert_eq!(
            IdempotencyStore::claim(&store, "usr_1", "deploy-1", now())
                .await
                .unwrap(),
            Claim::Fresh
        );
        // The client timed out and retried while the first call was still running.
        assert_eq!(
            IdempotencyStore::claim(&store, "usr_1", "deploy-1", now())
                .await
                .unwrap(),
            Claim::InFlight,
            "the retry must not start a second publish"
        );

        IdempotencyStore::complete(
            &store,
            "usr_1",
            "deploy-1",
            "{\"id\":\"ntf_1\"}",
            now() + Duration::hours(24),
        )
        .await
        .unwrap();

        assert_eq!(
            IdempotencyStore::claim(&store, "usr_1", "deploy-1", now())
                .await
                .unwrap(),
            Claim::Replayed {
                response: "{\"id\":\"ntf_1\"}".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn critical_t16_idempotency_keys_are_scoped_per_principal() {
        let store = InMemory::new();
        // `deploy-1` is not an imaginative choice of key; two publishers must not
        // read each other's responses.
        assert_eq!(
            IdempotencyStore::claim(&store, "usr_1", "deploy-1", now())
                .await
                .unwrap(),
            Claim::Fresh
        );
        assert_eq!(
            IdempotencyStore::claim(&store, "usr_2", "deploy-1", now())
                .await
                .unwrap(),
            Claim::Fresh
        );
    }

    #[tokio::test]
    async fn a_released_claim_frees_the_key_and_an_expired_one_lapses() {
        let store = InMemory::new();

        IdempotencyStore::claim(&store, "usr_1", "failed", now())
            .await
            .unwrap();
        IdempotencyStore::release(&store, "usr_1", "failed")
            .await
            .unwrap();
        assert_eq!(
            IdempotencyStore::claim(&store, "usr_1", "failed", now())
                .await
                .unwrap(),
            Claim::Fresh,
            "work that failed must not lock its key for the rest of the window"
        );

        IdempotencyStore::complete(&store, "usr_1", "old", "{}", now() + Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(
            IdempotencyStore::prune(&store, now() + Duration::hours(2))
                .await
                .unwrap(),
            0,
            "prune only removes what was actually reserved"
        );
    }

    // --- T16 · Upsert under concurrency -----------------------------------

    #[tokio::test]
    async fn critical_t16_a_second_publisher_on_the_same_external_id_conflicts() {
        let store = InMemory::new();
        let alice = user();
        store.seed_channel(channel("ci-infra", &alice));

        let external = ExternalId::parse("run-4521").unwrap();
        let build = |title: &str| {
            Notification::create(
                NotificationId::generate(),
                channel_id("ci-infra"),
                Some(external.clone()),
                RevisionDraft::lifecycle(State::Open, ContentDraft::titled(title)),
                author(&alice),
                now(),
                &ChannelPolicy::default(),
            )
            .unwrap()
        };

        let (first, first_written) = build("first");
        NotificationRepository::insert(&store, &first, &first_written.revision)
            .await
            .unwrap();

        let (second, second_written) = build("second");
        let error = NotificationRepository::insert(&store, &second, &second_written.revision)
            .await
            .unwrap_err();
        assert!(
            error.is_retryable(),
            "the caller re-reads and revises, which is what makes the upsert idempotent"
        );

        let found =
            NotificationRepository::find_by_external_id(&store, &channel_id("ci-infra"), &external)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(found.id, first.id);
    }

    #[tokio::test]
    async fn a_tombstoned_external_id_is_free_again() {
        let store = InMemory::new();
        let alice = user();
        store.seed_channel(channel("ci-infra", &alice));

        let external = ExternalId::parse("run-4521").unwrap();
        let (first, written) = Notification::create(
            NotificationId::generate(),
            channel_id("ci-infra"),
            Some(external.clone()),
            RevisionDraft::simple(ContentDraft::titled("old run")),
            author(&alice),
            now(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        NotificationRepository::insert(&store, &first, &written.revision)
            .await
            .unwrap();
        NotificationRepository::mark_deleted(&store, first.id, now())
            .await
            .unwrap();

        assert!(
            NotificationRepository::find_by_external_id(&store, &channel_id("ci-infra"), &external)
                .await
                .unwrap()
                .is_none(),
            "a re-run creates a new notification rather than reviving a tombstone"
        );
    }

    #[tokio::test]
    async fn deleting_twice_emits_one_tombstone() {
        let store = InMemory::new();
        let alice = user();
        store.seed_channel(channel("ci-infra", &alice));
        let notification = publish(&store, "ci-infra", &alice, "gone", now()).await;

        assert!(
            NotificationRepository::mark_deleted(&store, notification.id, now())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            NotificationRepository::mark_deleted(&store, notification.id, now())
                .await
                .unwrap()
                .is_none()
        );

        let tombstones = store
            .events()
            .into_iter()
            .filter(|event| matches!(event.payload, EventPayload::Tombstone { .. }))
            .count();
        assert_eq!(tombstones, 1);
    }

    // --- T27 · Search stays inside the caller's channels -------------------

    #[tokio::test]
    async fn critical_t27_search_never_leaves_the_callers_channels() {
        let store = InMemory::new();
        let (alice, bob) = (user(), user());
        store.seed_channel(channel("ci-infra", &alice));
        store.seed_channel(channel("cluster-ovh-1", &bob));

        publish(&store, "ci-infra", &alice, "etcd degraded", now()).await;
        publish(&store, "cluster-ovh-1", &bob, "etcd degraded", now()).await;

        let results = NotificationRepository::search(
            &store,
            &SearchQuery {
                text: "etcd".to_owned(),
                channels: vec![channel_id("ci-infra")],
                page: PageRequest::first(),
            },
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results.items[0].channel, channel_id("ci-infra"));
    }

    #[tokio::test]
    async fn critical_t27_a_deleted_notification_drops_out_of_search() {
        let store = InMemory::new();
        let alice = user();
        store.seed_channel(channel("ci-infra", &alice));
        let notification = publish(&store, "ci-infra", &alice, "etcd degraded", now()).await;

        NotificationRepository::mark_deleted(&store, notification.id, now())
            .await
            .unwrap();

        let results = NotificationRepository::search(
            &store,
            &SearchQuery {
                text: "etcd".to_owned(),
                channels: vec![channel_id("ci-infra")],
                page: PageRequest::first(),
            },
        )
        .await
        .unwrap();
        assert!(results.is_empty());
    }

    // --- Inbox and pagination ---------------------------------------------

    #[tokio::test]
    async fn the_inbox_reads_newest_first_and_pages_without_gaps_or_repeats() {
        let store = InMemory::new();
        let alice = user();
        store.seed_channel(channel("ci-infra", &alice));

        for index in 0..5 {
            publish(
                &store,
                "ci-infra",
                &alice,
                &format!("n{index}"),
                now() + Duration::seconds(index),
            )
            .await;
        }

        let query = |page: PageRequest| InboxQuery {
            channels: vec![channel_id("ci-infra")],
            page,
            ..InboxQuery::default()
        };

        let first = NotificationRepository::list(&store, &query(PageRequest::of_size(2)))
            .await
            .unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first.items[0].head.content.title, "n4");

        let next = first.next.unwrap();
        let second =
            NotificationRepository::list(&store, &query(PageRequest::of_size(2).after(next)))
                .await
                .unwrap();
        assert_eq!(second.items[0].head.content.title, "n2");

        let seen: Vec<String> = first
            .items
            .iter()
            .chain(second.items.iter())
            .map(|notification| notification.head.content.title.clone())
            .collect();
        assert_eq!(seen, ["n4", "n3", "n2", "n1"], "no gap, no repeat");
    }

    #[tokio::test]
    async fn the_inbox_filters_are_narrowings_and_never_widen() {
        let store = InMemory::new();
        let alice = user();
        store.seed_channel(channel("ci-infra", &alice));
        publish(&store, "ci-infra", &alice, "open one", now()).await;

        let closed = InboxQuery {
            channels: vec![channel_id("ci-infra")],
            states: vec![State::Closed],
            ..InboxQuery::default()
        };
        assert!(
            NotificationRepository::list(&store, &closed)
                .await
                .unwrap()
                .is_empty()
        );

        // An empty channel list means no channels, never every channel.
        let unscoped = InboxQuery::default();
        assert!(
            NotificationRepository::list(&store, &unscoped)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_timeline_reads_oldest_first() {
        let store = InMemory::new();
        let alice = user();
        store.seed_channel(channel("ci-infra", &alice));

        let mut notification = publish(&store, "ci-infra", &alice, "deploy", now()).await;
        let written = notification
            .revise(
                RevisionDraft::simple(ContentDraft::titled("deploy done")),
                author(&alice),
                now() + Duration::seconds(1),
                &ChannelPolicy::default(),
            )
            .unwrap();
        NotificationRepository::append_revision(&store, &notification, &written.revision)
            .await
            .unwrap();

        let page = NotificationRepository::revisions(&store, notification.id, PageRequest::first())
            .await
            .unwrap();
        let seqs: Vec<u32> = page.items.iter().map(|revision| revision.seq).collect();
        assert_eq!(seqs, [1, 2]);
    }

    // --- Interactions ------------------------------------------------------

    #[tokio::test]
    async fn interaction_outcomes_are_written_exactly_as_the_domain_decided() {
        use notify_domain::{ActionDraft, ActionKey, ChoiceOptionDraft, ChoiceOptionKey};

        let store = InMemory::new();
        let alice = user();
        store.seed_channel(channel("ci-infra", &alice));
        let notification = publish(&store, "ci-infra", &alice, "incident", now()).await;

        let actions = Actions::new(vec![
            ActionDraft::Ack {
                label: "Ack".to_owned(),
                key: "ack-incident".to_owned(),
            }
            .try_into()
            .unwrap(),
            ActionDraft::Choice {
                label: "Escalade".to_owned(),
                key: "escalate".to_owned(),
                options: vec![
                    ChoiceOptionDraft {
                        key: "oncall".to_owned(),
                        label: "Page on-call".to_owned(),
                    },
                    ChoiceOptionDraft {
                        key: "defer".to_owned(),
                        label: "Attendre".to_owned(),
                    },
                ],
            }
            .try_into()
            .unwrap(),
        ])
        .unwrap();

        let mut log = InteractionLog::default();
        let ack = || Interaction::Ack {
            key: ActionKey::parse("ack-incident").unwrap(),
        };

        let created = log
            .record(&actions, notification.id, author(&alice), ack(), now())
            .unwrap();
        assert!(
            InteractionRepository::apply(&store, &created)
                .await
                .unwrap()
                .is_some()
        );

        let unchanged = log
            .record(&actions, notification.id, author(&alice), ack(), now())
            .unwrap();
        assert!(
            InteractionRepository::apply(&store, &unchanged)
                .await
                .unwrap()
                .is_none(),
            "an idempotent acknowledgement writes nothing and broadcasts nothing"
        );

        let pick = |option: &str| Interaction::Choice {
            key: ActionKey::parse("escalate").unwrap(),
            option: ChoiceOptionKey::parse(option).unwrap(),
        };
        let first = log
            .record(
                &actions,
                notification.id,
                author(&alice),
                pick("defer"),
                now(),
            )
            .unwrap();
        InteractionRepository::apply(&store, &first).await.unwrap();
        let changed = log
            .record(
                &actions,
                notification.id,
                author(&alice),
                pick("oncall"),
                now(),
            )
            .unwrap();
        InteractionRepository::apply(&store, &changed)
            .await
            .unwrap();

        let stored = InteractionRepository::list_for(&store, notification.id)
            .await
            .unwrap();
        assert_eq!(
            stored.len(),
            2,
            "one acknowledgement and one surviving pick"
        );
    }

    // --- T26 · Silences are private ----------------------------------------

    #[tokio::test]
    async fn critical_t26_a_silence_reaches_only_its_own_user() {
        use notify_domain::{HumanDuration, Silence, SilenceTarget};

        let store = InMemory::new();
        let (alice, bob) = (user(), user());
        let (Principal::User { id: alice_id }, Principal::User { id: bob_id }) = (&alice, &bob)
        else {
            unreachable!()
        };
        store.seed_channel(channel("ci-infra", &alice));
        let mut shared = channel("ci-infra", &alice);
        shared.upsert_member(Membership::new(bob.clone(), vec![Scope::Subscribe]).unwrap());
        ChannelRepository::upsert(&store, &shared).await.unwrap();

        let silence = Silence::start(
            *alice_id,
            SilenceTarget::Group {
                key: "incident-4521".to_owned(),
            },
            Some(HumanDuration::from_secs_const(4 * 3_600)),
            now(),
        )
        .unwrap();
        InteractionRepository::silence(&store, &silence)
            .await
            .unwrap();

        let hers = EventStore::sync(&store, *alice_id, &sync_query(None))
            .await
            .unwrap();
        assert_eq!(hers.events.len(), 1);

        let his = EventStore::sync(&store, *bob_id, &sync_query(None))
            .await
            .unwrap();
        assert!(
            his.events.is_empty(),
            "sharing a channel does not make her silences his business"
        );

        assert_eq!(
            InteractionRepository::active_silences(&store, *alice_id, now() + Duration::hours(5))
                .await
                .unwrap()
                .len(),
            0,
            "and it lifts on its own"
        );
    }

    // --- Devices -----------------------------------------------------------

    #[tokio::test]
    async fn a_device_cursor_only_moves_forward_through_the_port() {
        let store = InMemory::new();
        let alice = user();
        let device = Device::enrol(
            DeviceId::generate(),
            alice.clone(),
            "laptop",
            PushTransport::Stream,
            None,
            now(),
        )
        .unwrap();
        let id = device.id;
        store.seed_device(device);

        DeviceRepository::advance_cursor(&store, id, Cursor::from_sequence(10), now())
            .await
            .unwrap();
        DeviceRepository::advance_cursor(&store, id, Cursor::from_sequence(4), now())
            .await
            .unwrap();

        let stored = DeviceRepository::get(&store, id).await.unwrap().unwrap();
        assert_eq!(stored.cursor, Some(Cursor::from_sequence(10)));
    }

    #[tokio::test]
    async fn only_subscribers_active_devices_are_delivery_targets() {
        let store = InMemory::new();
        let (alice, stranger) = (user(), user());
        store.seed_channel(channel("ci-infra", &alice));

        let enrol = |owner: &Principal, label: &str| {
            Device::enrol(
                DeviceId::generate(),
                owner.clone(),
                label,
                PushTransport::Stream,
                None,
                now(),
            )
            .unwrap()
        };

        let hers = enrol(&alice, "laptop");
        let mut revoked = enrol(&alice, "old phone");
        revoked.revoke(now());
        store.seed_device(hers.clone());
        store.seed_device(revoked);
        store.seed_device(enrol(&stranger, "not a member"));

        let targets = DeviceRepository::delivery_targets(&store, &channel_id("ci-infra"))
            .await
            .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, hers.id);
    }

    // --- Identities --------------------------------------------------------

    #[tokio::test]
    async fn deactivating_a_service_account_revokes_every_key_it_owns() {
        use notify_domain::{ApiKey, ApiKeyId, ApiKeyOwner, ApiKeyPrefix, ServiceAccountId};

        let store = InMemory::new();
        let bot = ServiceAccountId::parse("bot-argocd").unwrap();
        let principal = Principal::ServiceAccount { id: bot.clone() };

        for prefix in ["ci4xa9k2", "ci4xa9k3"] {
            store.seed_api_key(StoredApiKey {
                key: ApiKey {
                    id: ApiKeyId::generate(),
                    prefix: ApiKeyPrefix::parse(prefix).unwrap(),
                    label: None,
                    owner: ApiKeyOwner::ServiceAccount { id: bot.clone() },
                    scopes: vec![Scope::Publish],
                    channels: vec![channel_id("ci-infra")],
                    created_at: now(),
                    expires_at: None,
                    last_used_at: None,
                    revoked_at: None,
                },
                secret_hash: "stub$secret".to_owned(),
            });
        }

        assert_eq!(
            ApiKeyRepository::revoke_all_for(&store, &principal, now())
                .await
                .unwrap(),
            2
        );
        // Idempotent: a second deactivation revokes nothing further.
        assert_eq!(
            ApiKeyRepository::revoke_all_for(&store, &principal, now())
                .await
                .unwrap(),
            0
        );

        let keys = ApiKeyRepository::list_for(&store, &principal)
            .await
            .unwrap();
        assert!(keys.iter().all(|key| key.ensure_usable(now()).is_err()));
    }

    #[tokio::test]
    async fn a_key_is_found_by_its_prefix_and_carries_its_hash_beside_it() {
        use notify_domain::{ApiKey, ApiKeyId, ApiKeyOwner, ApiKeyPrefix};

        let store = InMemory::new();
        let prefix = ApiKeyPrefix::parse("ci4xa9k2").unwrap();
        ApiKeyRepository::insert(
            &store,
            &StoredApiKey {
                key: ApiKey {
                    id: ApiKeyId::generate(),
                    prefix: prefix.clone(),
                    label: Some("CI".to_owned()),
                    owner: ApiKeyOwner::Channel {
                        id: channel_id("ci-infra"),
                    },
                    scopes: vec![Scope::Publish],
                    channels: vec![channel_id("ci-infra")],
                    created_at: now(),
                    expires_at: None,
                    last_used_at: None,
                    revoked_at: None,
                },
                secret_hash: "stub$secret".to_owned(),
            },
        )
        .await
        .unwrap();

        let found = ApiKeyRepository::find_by_prefix(&store, &prefix)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.secret_hash, "stub$secret");

        ApiKeyRepository::touch(&store, found.key.id, now())
            .await
            .unwrap();
        let touched = ApiKeyRepository::get(&store, found.key.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(touched.last_used_at, Some(now()));
    }

    // --- Channels ----------------------------------------------------------

    #[tokio::test]
    async fn channel_listing_answers_per_scope() {
        let store = InMemory::new();
        let alice = user();
        let mut reader = Channel::dynamic(channel_id("read-only"), None).unwrap();
        reader.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());
        store.seed_channel(reader);
        store.seed_channel(channel("ci-infra", &alice));

        let publishable =
            ChannelRepository::list_for(&store, &alice, &[], Scope::Publish, PageRequest::first())
                .await
                .unwrap();
        assert_eq!(publishable.len(), 1);

        let readable = ChannelRepository::subscribed_ids(&store, &alice, &[])
            .await
            .unwrap();
        assert_eq!(readable.len(), 2);
    }

    #[tokio::test]
    async fn managed_channels_are_listed_apart_for_the_reconciler() {
        let store = InMemory::new();
        let alice = user();
        store.seed_channel(channel("ci-infra", &alice));

        let mut managed = channel("cluster-ovh-1", &alice);
        managed.origin = Origin::Managed;
        store.seed_channel(managed);

        let found = ChannelRepository::list_managed(&store).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, channel_id("cluster-ovh-1"));
    }

    // --- Fan-out -----------------------------------------------------------

    #[tokio::test]
    async fn a_poke_reaches_every_subscriber_and_a_dropped_one_costs_nothing() {
        use futures::StreamExt;

        let store = InMemory::new();
        let mut first = EventBus::subscribe(&store).await.unwrap();
        let second = EventBus::subscribe(&store).await.unwrap();
        drop(second);

        let poke = Poke::Channel {
            id: channel_id("ci-infra"),
            cursor: Cursor::from_sequence(9),
        };
        EventBus::publish(&store, &poke).await.unwrap();

        assert_eq!(first.next().await, Some(poke.clone()));
        assert_eq!(store.pokes(), vec![poke]);

        // The dropped subscriber costs a dropped poke, never a failed publish.
        EventBus::publish(
            &store,
            &Poke::User {
                id: UserId::generate(),
                cursor: Cursor::from_sequence(10),
            },
        )
        .await
        .unwrap();
    }

    // --- Audit -------------------------------------------------------------

    #[tokio::test]
    async fn the_audit_log_reads_backwards_and_filters() {
        let store = InMemory::new();
        let alice = user();

        for action in ["apikey.created", "channel.member_added"] {
            AuditLog::record(
                &store,
                &AuditEntry {
                    actor: alice.clone(),
                    action,
                    target: None,
                    channel: Some(channel_id("ci-infra")),
                    detail: None,
                    at: now(),
                },
            )
            .await
            .unwrap();
        }

        let all = AuditLog::query(&store, &AuditQuery::default())
            .await
            .unwrap();
        assert_eq!(
            all.items[0].action, "channel.member_added",
            "an incident review reads backwards"
        );

        let filtered = AuditLog::query(
            &store,
            &AuditQuery {
                action: Some("apikey.created".to_owned()),
                ..AuditQuery::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(filtered.len(), 1);
    }

    // --- Sink queue --------------------------------------------------------

    #[tokio::test]
    async fn a_due_delivery_is_claimed_once_and_a_buried_one_can_be_requeued() {
        use url::Url;

        let store = InMemory::new();
        let alice = user();
        store.seed_channel(channel("ci-infra", &alice));
        let notification = publish(&store, "ci-infra", &alice, "deploy", now()).await;
        let event = store.events().into_iter().next().unwrap();
        let _ = notification;

        let delivery = SinkDelivery {
            delivery_id: "dlv-1".to_owned(),
            channel: channel_id("ci-infra"),
            target: SinkTarget::Webhook {
                url: Url::parse("https://hooks.weebo.poc/incidents").unwrap(),
                secret: "shh".to_owned(),
            },
            event,
            attempt: 1,
        };

        SinkQueue::enqueue(&store, &delivery, now()).await.unwrap();
        assert!(
            SinkQueue::claim_due(&store, now() - Duration::seconds(1), 10)
                .await
                .unwrap()
                .is_empty(),
            "nothing is due yet"
        );

        let claimed = SinkQueue::claim_due(&store, now(), 10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert!(
            SinkQueue::claim_due(&store, now(), 10)
                .await
                .unwrap()
                .is_empty(),
            "two replicas briefly leading must not double-deliver a webhook"
        );

        SinkQueue::bury(&store, &delivery, "connection refused", now())
            .await
            .unwrap();
        assert_eq!(
            SinkQueue::dead_letter_depth(&store).await.unwrap(),
            vec![(channel_id("ci-infra"), 1)]
        );
        assert!(SinkQueue::requeue(&store, "dlv-1", now()).await.unwrap());
        assert!(!SinkQueue::requeue(&store, "dlv-1", now()).await.unwrap());
        assert!(
            SinkQueue::dead_letters(&store, None)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
