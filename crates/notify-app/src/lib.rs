//! Use cases: the application layer wiring the domain to its ports (design §4).
//!
//! One struct per use case, one `execute()`. Everything a use case needs arrives as
//! an `Arc<dyn Port>` in its constructor, so the composition root decides what is
//! real and a test decides what is in memory — and the code in between cannot tell.
//!
//! # What lives here and nowhere else
//!
//! **Authorisation.** Ports answer "who could", never "who may" (design §4), so
//! every membership and scope check funnels through [`authz`]. That is what makes
//! "membership is the only door to content" a property rather than a habit.
//!
//! **Orchestration across ports.** Publishing writes a notification, appends an
//! event, pokes the bus and pushes to devices. The domain knows none of that and
//! the adapters each know one piece; the use case is where the order is decided —
//! including which steps may fail without failing the request ([`fanout`]).
//!
//! **Quotas.** Consumed on the way *in*, before any work, so a caller over its
//! limit costs a counter increment rather than a transaction (design §8.2).
//!
//! # What deliberately does not
//!
//! No HTTP, no SQL, no framework types. A use case takes domain values and returns
//! domain values; an Actix extractor stops at the adapter's edge.
//!
//! # The M1 surface
//!
//! | Area | Use cases |
//! | --- | --- |
//! | Authentication | [`Authenticate`] |
//! | Publishing | [`PublishNotification`], [`UpsertNotification`], [`DeleteNotification`] |
//! | Reading | [`ListInbox`], [`GetNotification`], [`ListRevisions`], [`SearchNotifications`] |
//! | Interaction | [`RecordInteraction`], [`ListInteractions`], [`SilenceNotification`], [`PostReceipt`], [`ReadReceipts`] |
//! | Real time | [`SyncBacklog`], [`ResolveStreamStart`] |
//! | Administration | [`ListChannels`], [`ReadChannel`], [`CreateChannel`], [`ManageMembers`], [`DeleteChannel`], [`CreateApiKey`], [`ListApiKeys`], [`RevokeApiKey`], [`SetApiKeyLimit`], [`EnrolDevice`], [`ListDevices`], [`RevokeDevice`] |
//! | Background | [`PurgeExpired`] |

pub mod accounts;
pub mod attachments;
pub mod attribution;
pub mod auth;
pub mod authz;
pub mod channels;
pub mod devices;
pub mod digest;
pub mod fanout;
pub mod federate;
pub mod interact;
pub mod keys;
pub mod maintenance;
pub mod platform;
pub mod preferences;
pub mod profiles;
pub mod publish;
pub mod read;
pub mod sinks;
pub mod sync;

pub use accounts::{CreateServiceAccount, DeactivateServiceAccount, ListServiceAccounts};
pub use attachments::{Attachments, PresignAttachment, ReadAttachment, ReadAttachmentThumbnail};
pub use attribution::Attribution;
pub use auth::{Authenticate, Credential};
pub use authz::{Authorizer, Caller, Grant};
pub use channels::{
    CreateChannel, DeleteChannel, ListChannels, ManageMembers, ReadChannel, UpdateChannel,
};
pub use devices::{EnrolDevice, ListDevices, RevokeDevice};
pub use digest::{DigestReport, SendDndDigests};
pub use fanout::Fanout;
pub use federate::{ExchangeToken, LivePolicies};
pub use interact::{
    ListInteractions, PostReceipt, ReadReceipts, RecordInteraction, SilenceNotification,
};
pub use keys::{
    CreateApiKey, ListApiKeys, MintedApiKey, NewApiKey, RevokeApiKey, SetApiKeyLimit,
    UlidKeyGenerator,
};
pub use maintenance::{PurgeExpired, PurgeReport};
pub use platform::{
    BrowseAudit, ChannelCard, ChannelInventory, ListTrustPolicies, ReadReconciliation,
    Reconciliation, ReconciliationLog, SelfJoin, Trigger, TrustPolicyCard,
};
pub use preferences::{ReadDnd, SetDnd};
pub use profiles::{ReadAvatar, ReadProfile, SetAvatar, UpdateProfile};
pub use publish::{
    PublishNotification, PublishRequest, Published, UpsertNotification, UpsertRequest,
};
pub use read::{
    CountUnread, DeleteNotification, GetNotification, InboxFilters, ListInbox, ListRevisions,
    SearchNotifications,
};
pub use sinks::{DrainReport, DrainSinks, ListDeadLetters, RequeueDeadLetter};
pub use sync::{ResolveStreamStart, SyncBacklog, SyncRequest};
