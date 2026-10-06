//! Domain layer: entities, invariants and the notification state machine (§2).
//! Zero I/O, async or clock (time is passed in). Validation happens in constructors
//! and deserialisation alike, and nothing panics: failures are [`DomainError`]s.

pub mod action;
pub mod channel;
pub mod content;
pub mod cursor;
pub mod device;
pub mod dnd;
pub mod duration;
pub mod error;
pub mod federation;
pub mod hygiene;
pub mod identity;
pub mod ids;
pub mod interaction;
pub mod limits;
pub mod notification;
pub mod origin;
pub mod principal;
pub mod profile;
pub mod receipt;
pub mod silence;
pub mod sink;
pub mod ttl;

pub use action::{
    Action, ActionDraft, ActionKind, Actions, ChoiceOption, ChoiceOptionDraft, SilenceScope,
};
pub use channel::{Ceiling, Channel, ChannelPolicy, GroupBinding, Membership, PolicyEdit, Scope};
pub use content::{Content, ContentDraft, Image, Progress};
pub use cursor::Cursor;
pub use device::{Device, DevicePublicKey, PushTransport, check_push_endpoint, is_public_address};
pub use dnd::{Day, Dnd, DndDraft, DndScope, QuietHours, WallClock};
pub use duration::HumanDuration;
pub use error::{DomainError, ErrorKind, Result};
pub use federation::{
    Allowance, FederatedClaims, FederatedGrant, Pattern, TrustPolicies, TrustPolicy,
};
pub use identity::{ApiKey, ApiKeyPrefix, PresentedApiKey, ServiceAccount, User};
pub use ids::{
    ActionKey, ApiKeyId, AttachmentId, ChannelId, ChoiceOptionKey, DeliveryId, DeviceId,
    ExternalId, InteractionId, NotificationId, ReceiptId, RevisionId, ServiceAccountId, Slug,
    UserId,
};
pub use interaction::{Interaction, InteractionLog, InteractionRecord, Recorded};
pub use notification::{
    Notification, NotificationKind, Priority, Revision, RevisionDraft, State, Status,
    TombstoneReason, Written,
};
pub use origin::Origin;
pub use principal::{ApiKeyOwner, Author, Principal, PrincipalKind};
pub use profile::{FieldEdit, Profile, ProfileEdit, ProfileField};
pub use receipt::{Readership, Receipt, ReceiptKind, Sighting};
pub use silence::{Silence, SilenceTarget};
pub use sink::{RetryPolicy, SINK_KINDS, SinkId};
pub use ttl::{CloseAfter, Ttl, TtlPolicy, TtlResolution};
