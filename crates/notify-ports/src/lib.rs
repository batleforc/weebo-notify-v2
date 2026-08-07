//! Ports: the async traits through which the domain reaches the outside world (design §4).
//!
//! Every trait here is a promise the application layer is written against and an
//! adapter later keeps. Nothing in this crate talks to anything: it declares *what*
//! the outside world must be able to do, in the domain's own vocabulary, and the
//! composition root decides who does it.
//!
//! # Reading this crate
//!
//! * **Storage** — [`notifications`], [`channels`], [`devices`], [`receipts`],
//!   [`interactions`], [`identities`], [`idempotency`].
//! * **Distribution** — [`event`] (the log everything is read back from),
//!   [`eventbus`] (telling other replicas), [`push`], [`sinks`].
//! * **Infrastructure** — [`clock`], [`attachments`], [`tokens`], [`leader`],
//!   [`audit`].
//! * **Shared vocabulary** — [`error`], [`pagination`].
//!
//! # Four conventions that run through all of it
//!
//! **Two error types, on purpose.** [`error::PortError`] means the world was wrong
//! — a 5xx and a page. `notify_domain::DomainError` means the request was wrong — a
//! 4xx with a stable code. A use case that can only return one of them ends up
//! reporting dropped connections as validation failures.
//!
//! **Expected outcomes are values, not errors.** A push relay refusing, a webhook
//! consumer returning 500, a token failing to verify: those are things that happen
//! all day and belong in a metric. `Err` is reserved for "this port cannot do its
//! job at all".
//!
//! **Writes return their cursor.** Recording a change and appending its event row
//! are one transaction ([ADR-0012]), so the write is the only place that knows
//! where it landed. That cursor is what a client resumes from and what a fan-out
//! poke carries instead of a payload.
//!
//! **Ports do not authorise.** They answer "who could", never "who may". Membership
//! and scope checks live in the use cases, in one place, so that a new adapter
//! cannot quietly become a second policy engine.
//!
//! # Testing against ports
//!
//! The `test-utils` feature provides in-memory implementations of every trait here
//! (see [`test_utils`]), which is what makes `notify-domain` and `notify-app`
//! testable with no container, no clock and no network — and therefore what makes a
//! ≥ 90 % coverage gate on those two crates a reasonable thing to ask for
//! (design §3.2).
//!
//! [ADR-0012]: https://github.com/batleforc/weebo-notify-v2/blob/main/docs/content/docs/adr/0012-single-events-table-as-the-cursor.mdx

pub mod attachments;
pub mod audit;
pub mod channels;
pub mod clock;
pub mod devices;
pub mod error;
pub mod event;
pub mod eventbus;
pub mod idempotency;
pub mod identities;
pub mod interactions;
pub mod leader;
pub mod media;
pub mod metrics;
pub mod notifications;
pub mod pagination;
pub mod preferences;
pub mod push;
pub mod quota;
pub mod receipts;
// The two schemas that are written rather than derived (design §14.4, D1 and D3).
// Private: it contributes trait implementations, and there is nothing in it to call.
#[cfg(feature = "openapi")]
mod schema;
pub mod sinks;
pub mod tokens;

// Compiled for this crate's own tests too, so the doubles are exercised where they
// are written rather than only where they are used.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

pub use attachments::{
    AttachmentRegistry, AttachmentStore, PresignedDownload, PresignedUpload, StoredObject,
    UploadRequest,
};
pub use audit::{AUDIT_ACTIONS, AuditAction, AuditEntry, AuditLog, AuditQuery, actions};
pub use channels::ChannelRepository;
pub use clock::{Clock, SystemClock};
pub use devices::DeviceRepository;
pub use error::{Failure, PortError, PortResult};
pub use event::{
    Audience, Event, EventKind, EventPayload, EventStore, ExpiredWhileAway, PreferenceEvent,
    RevisionEvent, SyncPage, SyncQuery,
};
pub use eventbus::{EventBus, Poke};
pub use idempotency::{Claim, IdempotencyStore};
pub use identities::{
    ApiKeyRepository, KeyGenerator, SecretHasher, ServiceAccountRepository, StoredApiKey,
    UserRepository,
};
pub use interactions::InteractionRepository;
pub use leader::{LeaderElection, LeaderGuard};
pub use media::{AvatarProcessor, ImageRejection, ProcessedImage};
pub use metrics::{Metrics, NoMetrics, PushResult, StreamKind};
pub use notifications::{ExpiredNotification, InboxQuery, NotificationRepository, SearchQuery};
pub use pagination::{Page, PageRequest};
pub use preferences::{DeferredTally, PreferenceRepository};
pub use push::{PushBody, PushDispatcher, PushMessage, PushOutcome, PushTransport};
pub use quota::{Allowance, Quota, RateLimiter};
pub use receipts::ReceiptRepository;
pub use sinks::{
    DeadLetter, SinkDelivery, SinkDispatcher, SinkOutcome, SinkQueue, SinkRegistry, SinkTarget,
};
pub use tokens::{
    FederationVerifier, MintedClaims, MintedToken, ReplayGuard, TokenRejection, TokenVerifier,
    VerifiedClaims,
};
