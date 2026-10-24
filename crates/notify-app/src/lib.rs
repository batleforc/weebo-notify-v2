//! Use cases: the application layer wiring the domain to its ports (§4). One struct
//! per use case, ports injected as `Arc<dyn Port>`; authorisation lives in [`authz`],
//! cross-port orchestration here, and no HTTP, SQL or framework types.

pub mod accounts;
pub mod attachments;
pub mod attribution;
pub mod auth;
pub mod authz;
pub mod channels;
pub mod closure;
pub mod devices;
pub mod digest;
pub mod fanout;
pub mod federate;
pub mod interact;
pub mod keys;
pub mod live;
pub mod maintenance;
pub mod platform;
pub mod preferences;
pub mod profiles;
pub mod publish;
pub mod read;
pub mod sinks;
pub mod sync;

pub use accounts::{
    CreateServiceAccount, DeactivateServiceAccount, ListServiceAccounts, SearchUsers,
};
pub use attachments::{
    Attachments, PresignAttachment, ReadAttachment, ReadAttachmentThumbnail,
    is_allowed_attachment_type, normalised_content_type,
};
pub use attribution::Attribution;
pub use auth::{Authenticate, Credential, DEFAULT_GROUP_SNAPSHOT_TTL};
pub use authz::{Authorizer, Caller, Grant};
pub use channels::{
    CreateChannel, DeleteChannel, ListChannels, ManageMembers, ReadChannel, UpdateChannel,
};
pub use closure::{CloseStale, ClosureReport};
pub use devices::{DEFAULT_MAX_DEVICES_PER_USER, EnrolDevice, ListDevices, RevokeDevice};
pub use digest::{DigestReport, SendDndDigests};
pub use fanout::{DrainFanout, Fanout};
pub use federate::{ExchangeToken, LivePolicies};
pub use interact::{
    ListInteractions, PostReceipt, ReadReceipts, RecordInteraction, SilenceNotification,
};
pub use keys::{
    CreateApiKey, ListApiKeys, MintedApiKey, NewApiKey, OsRngKeyGenerator, RevokeApiKey,
    SetApiKeyLimit,
};
pub use live::{Live, LiveBatch, LiveFeed};
pub use maintenance::{EVENT_RETENTION, PurgeExpired, PurgeLimits, PurgeReport, TOMBSTONE_GRACE};
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
pub use sync::{ResolveStreamStart, SyncBacklog, SyncRequest, deferred_by};
