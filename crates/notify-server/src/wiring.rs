//! Building the application from the configuration (design §4).
//!
//! Every adapter is constructed exactly once here and handed to the use cases as
//! `Arc<dyn Port>`. Nothing below this file knows which adapter it got, which is
//! the whole point of the ports — and the reason the same use cases run against
//! `InMemory` in a sub-second test suite.
//!
//! One `PgStore` satisfies eleven ports. That is not a shortcut: several of them
//! have to write inside the *same* transaction as an event append (ADR-0012), and
//! splitting them into separate objects would mean either sharing a transaction
//! across objects or giving up the guarantee.

use std::sync::Arc;

use notify_app::{
    Authenticate, Authorizer, CreateApiKey, CreateChannel, CreateServiceAccount,
    DeactivateServiceAccount, DeleteChannel, DeleteNotification, EnrolDevice, Fanout,
    GetNotification, ListApiKeys, ListChannels, ListDevices, ListInbox, ListInteractions,
    ListRevisions, ListServiceAccounts, ManageMembers, PostReceipt, PublishNotification,
    PurgeExpired, ReadAvatar, ReadDnd, ReadProfile, RecordInteraction, ResolveStreamStart,
    RevokeApiKey, RevokeDevice, SearchNotifications, SendDndDigests, SetAvatar, SetDnd,
    SilenceNotification, SyncBacklog, UlidKeyGenerator, UpdateChannel, UpdateProfile,
    UpsertNotification,
};
use notify_attachments_s3::{S3Attachments, S3Config};
use notify_avatars::ImageAvatarProcessor;
use notify_eventbus_pg::PgEventBus;
use notify_http_actix::ApiState;
use notify_oidc::{Argon2SecretHasher, OidcConfig, OidcVerifier};
use notify_persistence_pg::{PgLeaderElection, PgStore};
use notify_ports::{AttachmentStore, EventBus, SystemClock};

use crate::bus::SharedBus;
use crate::config::Config;
use crate::push::TransportRegistry;
use crate::reconcile::Reconciler;

/// Everything the process needs, built once.
pub(crate) struct Application {
    /// What the HTTP layer is handed.
    pub state: ApiState,
    /// The store, for `/readyz` and for the migrations.
    pub store: PgStore,
    /// The one task holding this replica's `LISTEN` connection. Aborted before the
    /// runtime ends: a sqlx connection dropped afterwards panics in its destructor,
    /// and a panic in a destructor aborts rather than unwinds.
    pub pump: actix_web::rt::task::JoinHandle<()>,
    /// The TTL purge, run on a timer by whichever replica leads.
    pub purge: PurgeExpired,
    /// The do-not-disturb digest (§7.4), on the same terms.
    pub digests: SendDndDigests,
    /// GitOps reconciliation.
    pub reconciler: Reconciler,
    /// The push transports this deployment has — none in M1.
    pub transports: TransportRegistry,
}

impl core::fmt::Debug for Application {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Application")
            .field("transports", &self.transports)
            .finish_non_exhaustive()
    }
}

impl Application {
    /// Construct everything from the configuration.
    ///
    /// # Errors
    ///
    /// Fails if the database cannot be reached or the identity provider's issuer
    /// does not parse — both are operator problems, and both are better reported
    /// before the socket is bound than after.
    #[allow(
        clippy::too_many_lines,
        reason = "one statement per use case; splitting it would hide which port \
                  each one was handed, which is the only thing this function says"
    )]
    pub(crate) async fn build(config: &Config) -> anyhow::Result<Self> {
        let store = PgStore::connect(&config.database.url, config.database.max_connections).await?;
        // One LISTEN per replica, fanned out in process — see `bus`. Subscribing
        // per stream would open one Postgres connection per connected client.
        let (shared, pump) =
            SharedBus::start(Arc::new(PgEventBus::new(store.pool().clone()))).await?;
        let bus: Arc<dyn EventBus> = Arc::new(shared);

        let clock = Arc::new(SystemClock);
        let repository = Arc::new(store.clone());

        // The identity provider. `audiences` always includes the client id, because
        // that is what an access token minted for this client carries; the extra
        // ones exist for issuers that name the API instead (§5.1).
        let mut oidc = OidcConfig::new(config.oidc.issuer.parse()?, config.oidc.client_id.clone());
        oidc.audiences.extend(config.oidc.audiences.iter().cloned());
        oidc.groups_claim.clone_from(&config.oidc.groups_claim);
        let verifier = Arc::new(OidcVerifier::new(oidc, clock.clone())?);
        let hasher = Arc::new(Argon2SecretHasher);

        let transports = TransportRegistry::empty();
        let dispatcher = Arc::new(transports.clone());

        // The object store. Absent from the configuration means absent from the
        // deployment, and the double that stands in for it refuses rather than
        // pretends: an avatar upload then answers "not configured" instead of
        // succeeding into nowhere (§14.3 — a port with no adapter is a refusal, not
        // a silent no-op).
        let attachments: Arc<dyn AttachmentStore> = match &config.attachments {
            Some(settings) => Arc::new(S3Attachments::connect(&S3Config {
                endpoint: settings.endpoint.parse()?,
                bucket: settings.bucket.clone(),
                region: settings.region.clone(),
                access_key: settings.access_key.clone(),
                secret_key: settings.secret_key.clone(),
            })?),
            None => Arc::new(NoAttachments),
        };
        let avatars = Arc::new(ImageAvatarProcessor::new());

        let authorizer = Authorizer::new(repository.clone());
        let fanout = Fanout::new(
            bus.clone(),
            repository.clone(),
            dispatcher,
            repository.clone(),
            repository.clone(),
            clock.clone(),
        );

        // Shared by everything that has to answer "is this notification still
        // there, and may this caller see it".
        let get = GetNotification::new(repository.clone(), authorizer.clone(), clock.clone());

        let state = ApiState {
            bus: bus.clone(),
            authenticate: Authenticate::new(
                verifier,
                repository.clone(),
                repository.clone(),
                repository.clone(),
                hasher.clone(),
                clock.clone(),
            ),

            publish: PublishNotification::new(
                repository.clone(),
                repository.clone(),
                repository.clone(),
                clock.clone(),
                authorizer.clone(),
                fanout.clone(),
            ),
            upsert: UpsertNotification::new(
                repository.clone(),
                repository.clone(),
                repository.clone(),
                clock.clone(),
                authorizer.clone(),
                fanout.clone(),
            ),
            delete_notification: DeleteNotification::new(
                repository.clone(),
                authorizer.clone(),
                clock.clone(),
                fanout.clone(),
            ),

            inbox: ListInbox::new(repository.clone(), authorizer.clone()),
            notification: get.clone(),
            revisions: ListRevisions::new(repository.clone(), get.clone()),
            search: SearchNotifications::new(
                repository.clone(),
                authorizer.clone(),
                repository.clone(),
                clock.clone(),
            ),

            record_interaction: RecordInteraction::new(
                repository.clone(),
                get.clone(),
                clock.clone(),
                fanout.clone(),
            ),
            interactions: ListInteractions::new(repository.clone(), get.clone()),
            silence: SilenceNotification::new(
                repository.clone(),
                get.clone(),
                clock.clone(),
                fanout.clone(),
            ),
            receipt: PostReceipt::new(
                repository.clone(),
                repository.clone(),
                repository.clone(),
                get,
                clock.clone(),
                fanout.clone(),
            ),

            sync: SyncBacklog::new(
                repository.clone(),
                repository.clone(),
                repository.clone(),
                authorizer.clone(),
                clock.clone(),
            ),
            stream_start: ResolveStreamStart::new(repository.clone(), repository.clone()),

            channels: ListChannels::new(repository.clone()),
            create_channel: CreateChannel::new(
                repository.clone(),
                repository.clone(),
                clock.clone(),
            ),
            update_channel: UpdateChannel::new(
                repository.clone(),
                authorizer.clone(),
                repository.clone(),
                clock.clone(),
            ),
            delete_channel: DeleteChannel::new(
                repository.clone(),
                authorizer.clone(),
                repository.clone(),
                clock.clone(),
            ),
            members: ManageMembers::new(
                repository.clone(),
                authorizer.clone(),
                repository.clone(),
                clock.clone(),
            ),

            create_key: CreateApiKey::new(
                repository.clone(),
                hasher,
                Arc::new(UlidKeyGenerator),
                authorizer,
                repository.clone(),
                clock.clone(),
            ),
            keys: ListApiKeys::new(repository.clone()),
            revoke_key: RevokeApiKey::new(repository.clone(), repository.clone(), clock.clone()),

            enrol_device: EnrolDevice::new(repository.clone(), repository.clone(), clock.clone()),
            devices: ListDevices::new(repository.clone()),
            revoke_device: RevokeDevice::new(repository.clone(), repository.clone(), clock.clone()),

            profile: ReadProfile::new(repository.clone()),
            update_profile: UpdateProfile::new(repository.clone()),
            set_avatar: SetAvatar::new(repository.clone(), avatars, attachments.clone()),
            avatar: ReadAvatar::new(attachments),

            dnd: ReadDnd::new(repository.clone()),
            set_dnd: SetDnd::new(repository.clone(), clock.clone(), fanout.clone()),

            accounts: ListServiceAccounts::new(repository.clone()),
            create_account: CreateServiceAccount::new(
                repository.clone(),
                repository.clone(),
                clock.clone(),
            ),
            deactivate_account: DeactivateServiceAccount::new(
                repository.clone(),
                repository.clone(),
                repository.clone(),
                clock.clone(),
            ),
        };

        let purge = PurgeExpired::new(
            repository.clone(),
            repository.clone(),
            repository.clone(),
            Arc::new(PgLeaderElection::new(store.pool().clone())),
            bus.clone(),
            clock.clone(),
        );

        // The digest of §7.4: one push when a quiet period ends, leader-only for the
        // same reason the purge is — every replica running it would send one each.
        let digests = SendDndDigests::new(
            repository.clone(),
            repository.clone(),
            Arc::new(PgLeaderElection::new(store.pool().clone())),
            fanout,
            clock.clone(),
        );

        let reconciler = Reconciler::new(repository.clone(), repository.clone(), repository, clock);

        Ok(Self {
            state,
            store,
            pump,
            purge,
            digests,
            reconciler,
            transports,
        })
    }
}

/// The object store a deployment without one gets.
///
/// It refuses rather than pretending. §14.3 makes the rule explicit: a port with no
/// adapter answers `NotConfigured` naming the capability, so an avatar upload on an
/// instance with no bucket fails visibly instead of succeeding into nowhere and
/// leaving a profile pointing at bytes that were never written.
#[derive(Debug)]
struct NoAttachments;

#[async_trait::async_trait]
impl AttachmentStore for NoAttachments {
    async fn presign_upload(
        &self,
        _request: &notify_ports::UploadRequest,
    ) -> notify_ports::PortResult<notify_ports::PresignedUpload> {
        Err(unconfigured())
    }

    async fn presign_download(
        &self,
        _id: notify_domain::AttachmentId,
    ) -> notify_ports::PortResult<notify_ports::PresignedDownload> {
        Err(unconfigured())
    }

    async fn exists(&self, _id: notify_domain::AttachmentId) -> notify_ports::PortResult<bool> {
        Ok(false)
    }

    async fn delete(&self, _id: notify_domain::AttachmentId) -> notify_ports::PortResult<()> {
        // Nothing to delete is the desired state, so the purge job stays green on an
        // instance that never stored anything.
        Ok(())
    }

    async fn put(
        &self,
        _id: notify_domain::AttachmentId,
        _content_type: &str,
        _bytes: Vec<u8>,
    ) -> notify_ports::PortResult<()> {
        Err(unconfigured())
    }

    async fn get(
        &self,
        _id: notify_domain::AttachmentId,
    ) -> notify_ports::PortResult<Option<notify_ports::StoredObject>> {
        Ok(None)
    }

    async fn health(&self) -> notify_ports::PortResult<()> {
        // Healthy: an instance with no bucket configured is not a broken instance,
        // and reporting it unready would take the whole replica out over a feature
        // it was never asked to provide.
        Ok(())
    }
}

const fn unconfigured() -> notify_ports::PortError {
    notify_ports::PortError::NotConfigured {
        what: "attachments",
    }
}
