//! Ports: the async traits through which the domain reaches the outside world (§4).
//! `PortError` is the world failing (5xx), `DomainError` the request (4xx); expected
//! outcomes are values; writes return their cursor (ADR-0012); ports never authorise.

pub mod attachments;
pub mod audit;
pub mod channels;
pub mod clock;
pub mod declarations;
pub mod devices;
pub mod error;
pub mod event;
pub mod eventbus;
pub mod feeds;
pub mod idempotency;
pub mod identities;
pub mod interactions;
pub mod leader;
pub mod media;
pub mod metrics;
pub mod notifications;
pub mod outbox;
pub mod pagination;
pub mod preferences;
pub mod push;
pub mod quota;
pub mod receipts;
// Hand-written schemas (§14.4); contributes trait impls only.
#[cfg(feature = "openapi")]
mod schema;
pub mod sinks;
pub mod tokens;
pub mod transforms;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

pub use attachments::{
    AttachmentRegistry, AttachmentStore, PresignedDownload, PresignedUpload, StoredObject,
    UploadRequest,
};
pub use audit::{
    AUDIT_ACTIONS, AUDIT_RETENTION_FLOOR, AuditAction, AuditActor, AuditEntry, AuditLog,
    AuditQuery, Configured, actions,
};
pub use channels::ChannelRepository;
pub use clock::{Clock, SystemClock};
pub use declarations::{Changed, Changes, Declaration, DeclarationKind, DeclarationStore};
pub use devices::DeviceRepository;
pub use error::{Failure, PortError, PortResult};
pub use event::{
    Audience, Event, EventKind, EventPayload, EventStore, ExpiredWhileAway, PreferenceEvent,
    RevisionEvent, SyncPage, SyncQuery,
};
pub use eventbus::{EventBus, Poke};
pub use feeds::{
    DeclaredFeed, FEED_KEY_HEX, FeedEntry, FeedFetcher, FeedKey, FeedLang, FeedRegistry,
    FeedRequest, FeedState, FeedStateStore, FetchError, Fetched, FetchedFeed, PendingItem,
    TOUCH_EVERY,
};
pub use idempotency::{Claim, IdempotencyStore};
pub use identities::{
    ApiKeyRepository, EmailMatch, KeyGenerator, SecretHasher, ServiceAccountRepository,
    StoredApiKey, UserRepository,
};
pub use interactions::InteractionRepository;
pub use leader::{LeaderElection, LeaderGuard};
pub use media::{AttachmentThumbnailer, AvatarProcessor, ImageRejection, ProcessedImage};
pub use metrics::{FanoutBacklog, Metrics, NoMetrics, PushResult, RetentionBacklog, StreamKind};
pub use notifications::{
    ClosedNotification, ExpiredNotification, InboxQuery, NotificationRepository, SearchQuery,
    UnreadCount,
};
pub use outbox::{AfterWrite, FanoutOutbox, IdempotentReply, PendingFanout};
pub use pagination::{Page, PageRequest};
pub use preferences::{DeferredTally, PreferenceRepository};
pub use push::{
    PushBody, PushDispatcher, PushMessage, PushOutcome, PushTransport, SEALED_ENVELOPE_VERSION,
};
pub use quota::{Acquired, Allowance, Lease, LeaseRequest, Quota, RateLimiter, StreamLeases};
pub use receipts::{ReceiptRepository, Visibility};
pub use sinks::{
    DeadLetter, FingerprintKey, SinkDelivery, SinkDispatcher, SinkOutcome, SinkQueue, SinkRegistry,
    SinkTarget, redact_url,
};
pub use tokens::{
    FederationVerifier, MintedClaims, MintedToken, ReplayGuard, TokenRejection, TokenVerifier,
    VerifiedClaims,
};
pub use transforms::{
    Mapped, RawMode, Transform, TransformError, TransformRegistry, element_digest,
};
