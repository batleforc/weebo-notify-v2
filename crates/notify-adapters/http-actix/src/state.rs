//! Everything a handler is allowed to reach.
//!
//! One struct holding one use case per operation, built by the composition root.
//! Deliberately not "the ports": a handler that could reach a repository directly
//! is a handler that can skip an authorisation check, and the whole point of
//! [`notify_app`] is that it cannot.

use std::sync::Arc;

use notify_app::{
    Authenticate, BrowseAudit, ChannelInventory, CreateApiKey, CreateChannel, CreateServiceAccount,
    DeactivateServiceAccount, DeleteChannel, DeleteNotification, EnrolDevice, ExchangeToken,
    GetNotification, ListApiKeys, ListChannels, ListDeadLetters, ListDevices, ListInbox,
    ListInteractions, ListRevisions, ListServiceAccounts, ListTrustPolicies, ManageMembers,
    PostReceipt, PresignAttachment, PublishNotification, ReadAttachment, ReadAvatar, ReadChannel,
    ReadDnd, ReadProfile, ReadReceipts, ReadReconciliation, RecordInteraction, RequeueDeadLetter,
    ResolveStreamStart, RevokeApiKey, RevokeDevice, SearchNotifications, SelfJoin, SetAvatar,
    SetDnd, SilenceNotification, SyncBacklog, UpdateChannel, UpdateProfile, UpsertNotification,
};
use notify_ports::{EventBus, Metrics};

/// The application, as the HTTP layer sees it.
#[derive(Clone)]
pub struct ApiState {
    /// The one port this layer holds directly: a live connection has to learn that
    /// something landed, and "subscribe to pokes" is a transport concern rather
    /// than a use case. What it does *with* a poke still goes through
    /// [`ApiState::sync`], so nothing here decides who may see what.
    pub bus: Arc<dyn EventBus>,

    /// What this replica is measured on (design §13). Held here rather than passed
    /// to the use cases: every number below is a fact about the process — how many
    /// connections it holds, how far behind a client was — and a use case that
    /// recorded its own would be one that knows it is being watched.
    pub metrics: Arc<dyn Metrics>,

    /// The live connections this replica holds, so `GET /stream` and `GET /ws` can
    /// enforce the ceiling of design §8.2.
    ///
    /// Here for the same reason `metrics` is: it is a fact about *this process*, not
    /// a decision about who may see what. `Arc` and not a plain field because
    /// [`ApiState`] is cloned into every worker and one shared count is the point —
    /// twenty per worker would be twenty times the ceiling.
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

    /// `POST /auth/token/exchange` — absent on a deployment with no `[federation]`
    /// block, which the route reports as `501` rather than as a bad token.
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

    /// `GET /push/webpush` — the VAPID public key, base64url, or `None` on a
    /// deployment that runs no Web Push (design §6.2, §11 `[push.webpush]`).
    ///
    /// The one field here that is not a use case, and it is not one because there is
    /// nothing to decide: it is a constant of the deployment, public by definition —
    /// every subscriber holds a copy — and it reaches no port. A use case wrapping a
    /// `String` would have been ceremony around a `clone`, and the rule this bends
    /// exists to stop a handler reaching a *repository*, which this cannot.
    pub application_server_key: Option<String>,

    /// `GET /serviceaccounts`.
    pub accounts: ListServiceAccounts,
    /// `POST /serviceaccounts`.
    pub create_account: CreateServiceAccount,
    /// `DELETE /serviceaccounts/{id}`.
    pub deactivate_account: DeactivateServiceAccount,
}

impl core::fmt::Debug for ApiState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // A trait object has no useful debug shape, and printing two dozen
        // `finish_non_exhaustive` use cases helps nobody.
        f.debug_struct("ApiState").finish_non_exhaustive()
    }
}
