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
    Authenticate, Authorizer, CreateApiKey, CreateChannel, DeleteChannel, DeleteNotification,
    EnrolDevice, Fanout, GetNotification, ListApiKeys, ListChannels, ListDevices, ListInbox,
    ListInteractions, ListRevisions, ManageMembers, PostReceipt, PublishNotification, PurgeExpired,
    RecordInteraction, ResolveStreamStart, RevokeApiKey, RevokeDevice, SearchNotifications,
    SilenceNotification, SyncBacklog, UlidKeyGenerator, UpsertNotification,
};
use notify_eventbus_pg::PgEventBus;
use notify_http_actix::ApiState;
use notify_oidc::{Argon2SecretHasher, OidcConfig, OidcVerifier};
use notify_persistence_pg::{PgLeaderElection, PgStore};
use notify_ports::{EventBus, SystemClock};

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

        let authorizer = Authorizer::new(repository.clone());
        let fanout = Fanout::new(
            bus.clone(),
            repository.clone(),
            dispatcher,
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
                fanout,
            ),

            sync: SyncBacklog::new(
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
        };

        let purge = PurgeExpired::new(
            repository.clone(),
            repository.clone(),
            repository.clone(),
            Arc::new(PgLeaderElection::new(store.pool().clone())),
            bus.clone(),
            clock.clone(),
        );

        let reconciler = Reconciler::new(repository.clone(), repository.clone(), repository, clock);

        Ok(Self {
            state,
            store,
            pump,
            purge,
            reconciler,
            transports,
        })
    }
}
