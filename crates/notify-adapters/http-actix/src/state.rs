//! Everything a handler may reach: use cases, not ports, so no handler can skip an
//! authorisation check.

use std::sync::Arc;

use notify_app::{
    Authenticate, BrowseAudit, ChannelInventory, CountUnread, CreateApiKey, CreateChannel,
    CreateServiceAccount, DeactivateServiceAccount, DeleteChannel, DeleteNotification, EnrolDevice,
    ExchangeToken, GetNotification, ListApiKeys, ListChannels, ListDeadLetters, ListDevices,
    ListInbox, ListInteractions, ListRevisions, ListServiceAccounts, ListTrustPolicies,
    ManageMembers, PostReceipt, PresignAttachment, PublishNotification, ReadAttachment,
    ReadAttachmentThumbnail, ReadAvatar, ReadChannel, ReadDnd, ReadProfile, ReadReceipts,
    ReadReconciliation, RecordInteraction, RequeueDeadLetter, ResolveStreamStart, RevokeApiKey,
    RevokeDevice, SearchNotifications, SearchUsers, SelfJoin, SetApiKeyLimit, SetAvatar, SetDnd,
    SilenceNotification, SyncBacklog, UpdateChannel, UpdateProfile, UpsertNotification,
};
use notify_attachments_fs::FsAttachments;
use notify_ports::Metrics;

/// The application, as the HTTP layer sees it.
#[derive(Clone)]
pub struct ApiState {
    /// The replica's one reader of the event log, which live connections listen to; a
    /// connection behind it reads via [`ApiState::sync`].
    pub live: notify_app::LiveFeed,

    /// Process-level metrics (design §13).
    pub metrics: Arc<dyn Metrics>,

    /// Live connections for the §8.2 ceiling; one `Arc` shared by every worker so the
    /// ceiling is per replica, not per worker.
    pub streams: Arc<crate::slots::StreamSlots>,

    /// Credential to caller, for the extractor.
    pub authenticate: Authenticate,

    /// `POST /notifications`.
    pub publish: PublishNotification,
    /// `PUT /channels/{id}/notifications/{external_id}`.
    pub upsert: UpsertNotification,
    /// `DELETE /notifications/{id}`.
    pub delete_notification: DeleteNotification,

    /// `GET /channels/{id}/notifications`.
    pub inbox: ListInbox,
    /// `GET /notifications/{id}`.
    pub notification: GetNotification,
    /// `GET /notifications/{id}/revisions`.
    pub revisions: ListRevisions,
    /// `GET /search`.
    pub search: SearchNotifications,
    /// `GET /unread`.
    pub unread: CountUnread,

    /// `POST /notifications/{id}/interactions`.
    pub record_interaction: RecordInteraction,
    /// `GET /notifications/{id}/interactions`.
    pub interactions: ListInteractions,
    /// `POST /notifications/{id}/silence`.
    pub silence: SilenceNotification,
    /// `POST /receipts`.
    pub receipt: PostReceipt,
    /// `GET /notifications/{id}/receipts`.
    pub readership: ReadReceipts,

    /// `POST /auth/token/exchange`; `None` without `[federation]` (answers `501`).
    pub exchange: Option<ExchangeToken>,

    /// `GET /admin/audit`.
    pub audit: BrowseAudit,
    /// `GET /admin/channels`.
    pub inventory: ChannelInventory,
    /// `POST /admin/channels/{channel}/join`.
    pub self_join: SelfJoin,
    /// `GET /admin/trust-policies`.
    pub trust_policies: ListTrustPolicies,
    /// `GET /admin/reconciliation`.
    pub reconciliation: ReadReconciliation,
    /// `GET /admin/dlq` — the sink deliveries that were given up on (§7.1bis).
    pub dlq: ListDeadLetters,
    /// `POST /admin/dlq/{id}/requeue`.
    pub requeue: RequeueDeadLetter,

    /// `POST /attachments/presign`.
    pub presign: PresignAttachment,
    /// `GET /attachments/{id}`.
    pub attachment: ReadAttachment,
    /// `GET /attachments/{id}/thumbnail`.
    pub attachment_thumbnail: ReadAttachmentThumbnail,

    /// The filesystem store when configured (ADR-0022); concrete because its routes
    /// need `FsAttachments::verify`, which is not on the trait.
    pub attachments_fs: Option<Arc<FsAttachments>>,

    /// `GET /sync`.
    pub sync: SyncBacklog,
    /// Where `GET /stream` and `GET /ws` resume from.
    pub stream_start: ResolveStreamStart,

    /// `GET /channels`.
    pub channels: ListChannels,
    /// `GET /channels/{id}`.
    pub read_channel: ReadChannel,
    /// `POST /channels`.
    pub create_channel: CreateChannel,
    /// `PATCH /channels/{id}`.
    pub update_channel: UpdateChannel,
    /// `DELETE /channels/{id}`.
    pub delete_channel: DeleteChannel,
    /// `POST`/`DELETE /channels/{id}/members`.
    pub members: ManageMembers,

    /// `POST /apikeys`.
    pub create_key: CreateApiKey,
    /// `GET /apikeys`.
    pub keys: ListApiKeys,
    /// `DELETE /apikeys/{id}`.
    pub revoke_key: RevokeApiKey,
    /// `PATCH /apikeys/{id}/limit`.
    pub set_key_limit: SetApiKeyLimit,

    /// `POST /devices`.
    pub enrol_device: EnrolDevice,
    /// `GET /devices`.
    pub devices: ListDevices,
    /// `DELETE /devices/{id}`.
    pub revoke_device: RevokeDevice,

    /// `GET /users/me/profile`.
    pub profile: ReadProfile,
    /// `PUT /users/me/profile`.
    pub update_profile: UpdateProfile,
    /// `PUT /users/me/avatar`.
    pub set_avatar: SetAvatar,
    /// `GET /avatars/{id}`.
    pub avatar: ReadAvatar,

    /// `GET /preferences/dnd`.
    pub dnd: ReadDnd,
    /// `PUT /preferences/dnd`.
    pub set_dnd: SetDnd,

    /// `GET /push/webpush`: the public VAPID key (base64url), `None` without Web Push (§6.2).
    pub application_server_key: Option<String>,

    /// `GET /serviceaccounts`.
    pub accounts: ListServiceAccounts,
    /// `GET /users`.
    pub users: SearchUsers,
    /// `POST /serviceaccounts`.
    pub create_account: CreateServiceAccount,
    /// `DELETE /serviceaccounts/{id}`.
    pub deactivate_account: DeactivateServiceAccount,

    /// `GET /cli`: the `notify` binary from `[cli]`, read once at boot; `None` answers `501`.
    pub cli_binary: Option<actix_web::web::Bytes>,
    /// SHA-256 of [`Self::cli_binary`] (hex), served as `X-Content-Digest`/`ETag`.
    /// An integrity check, **not a signature**.
    pub cli_digest: Option<String>,
}

impl core::fmt::Debug for ApiState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ApiState").finish_non_exhaustive()
    }
}
