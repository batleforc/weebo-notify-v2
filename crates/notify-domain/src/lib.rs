//! Domain layer: entities, invariants and the notification state machine (design §2).
//!
//! Zero I/O, zero async, zero clock. Everything here is a pure function of its
//! inputs — including the time, which is passed in rather than read (the `Clock`
//! port, design §4). That is what makes the rules that matter testable without a
//! database, a container or a sleep, and it is why the coverage bar is highest
//! here (design §3.2).
//!
//! # The shape of it
//!
//! * [`notification`] — the aggregate and its state machine: birth, revision,
//!   closure, reopening, retention. The centre of the model.
//! * [`content`] and [`action`] — what a notification says and what it offers to
//!   do. Constructible only through validation.
//! * [`channel`] — membership, scopes and policy: the only door to content.
//! * [`interaction`], [`receipt`], [`silence`] — what comes back from a
//!   notification, and how a user turns it down.
//! * [`principal`] and [`identity`] — who is acting, and who exists to act.
//! * [`device`] and [`cursor`] — where a notification is delivered, and how far
//!   each device has read.
//! * [`hygiene`], [`ids`], [`duration`], [`ttl`], [`origin`], [`limits`],
//!   [`error`] — the shared vocabulary the rest is written in.
//!
//! # Two habits worth knowing before reading
//!
//! **Validation is a constructor, not a step.** There is no way to hold a
//! [`Content`] or an [`Action`] that has not been sanitised and bounds-checked —
//! including one that came back out of Postgres, because deserialisation runs the
//! same validation as ingestion. Storage is not a trust boundary we get to skip: a
//! row written by last month's build must still satisfy this month's invariants.
//!
//! **Nothing here panics.** `unwrap`, `expect` and `panic!` are denied
//! workspace-wide: a malformed payload has to become a 4xx, never a downed replica.
//! Fallible operations return [`DomainError`], which carries a stable `code` that
//! clients match on.

pub mod action;
pub mod channel;
pub mod content;
pub mod cursor;
pub mod device;
pub mod duration;
pub mod error;
pub mod hygiene;
pub mod identity;
pub mod ids;
pub mod interaction;
pub mod limits;
pub mod notification;
pub mod origin;
pub mod principal;
pub mod receipt;
pub mod silence;
pub mod ttl;

pub use action::{
    Action, ActionDraft, ActionKind, Actions, ChoiceOption, ChoiceOptionDraft, SilenceScope,
};
pub use channel::{Channel, ChannelPolicy, Membership, Scope};
pub use content::{Content, ContentDraft, Image, Progress};
pub use cursor::Cursor;
pub use device::{Device, DevicePublicKey, PushTransport};
pub use duration::HumanDuration;
pub use error::{DomainError, ErrorKind, Result};
pub use identity::{ApiKey, ApiKeyPrefix, PresentedApiKey, ServiceAccount, User};
pub use ids::{
    ActionKey, ApiKeyId, AttachmentId, ChannelId, ChoiceOptionKey, DeviceId, ExternalId,
    InteractionId, NotificationId, ReceiptId, RevisionId, ServiceAccountId, Slug, UserId,
};
pub use interaction::{Interaction, InteractionLog, InteractionRecord, Recorded};
pub use notification::{
    Notification, NotificationKind, Priority, Revision, RevisionDraft, State, Status,
    TombstoneReason, Written,
};
pub use origin::Origin;
pub use principal::{ApiKeyOwner, Author, Principal, PrincipalKind};
pub use receipt::{Receipt, ReceiptKind, SeenTally};
pub use silence::{Silence, SilenceTarget};
pub use ttl::{Ttl, TtlPolicy, TtlResolution};
