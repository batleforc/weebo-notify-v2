//! Everything a handler is allowed to reach.
//!
//! One struct holding one use case per operation, built by the composition root.
//! Deliberately not "the ports": a handler that could reach a repository directly
//! is a handler that can skip an authorisation check, and the whole point of
//! [`notify_app`] is that it cannot.

use std::sync::Arc;

use notify_app::{
    Authenticate, CreateApiKey, CreateChannel, DeleteChannel, DeleteNotification, EnrolDevice,
    GetNotification, ListApiKeys, ListChannels, ListDevices, ListInbox, ListInteractions,
    ListRevisions, ManageMembers, PostReceipt, PublishNotification, RecordInteraction,
    ResolveStreamStart, RevokeApiKey, RevokeDevice, SearchNotifications, SilenceNotification,
    SyncBacklog, UpsertNotification,
};
use notify_ports::EventBus;

/// The application, as the HTTP layer sees it.
#[derive(Clone)]
pub struct ApiState {
    /// The one port this layer holds directly: a live connection has to learn that
    /// something landed, and "subscribe to pokes" is a transport concern rather
    /// than a use case. What it does *with* a poke still goes through
    /// [`ApiState::sync`], so nothing here decides who may see what.
    pub bus: Arc<dyn EventBus>,

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

    /// `GET /sync`.
    pub sync: SyncBacklog,
    /// Where `GET /stream` and `GET /ws` resume from.
    pub stream_start: ResolveStreamStart,

    /// `GET /channels`.
    pub channels: ListChannels,
    /// `POST /channels`.
    pub create_channel: CreateChannel,
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
}

impl core::fmt::Debug for ApiState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // A trait object has no useful debug shape, and printing two dozen
        // `finish_non_exhaustive` use cases helps nobody.
        f.debug_struct("ApiState").finish_non_exhaustive()
    }
}
