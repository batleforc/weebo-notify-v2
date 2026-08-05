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
use notify_push_webpush::{WebPush, WebPushConfig};

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
    /// The push transports this deployment configured, for `/readyz` and the
    /// startup log.
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

        // The transports `[push.*]` asks for. Nothing above this line knows which
        // ones there are: a deployment with no `[push.webpush]` gets a registry that
        // names the missing capability, and the fan-out carries on (§14.3).
        let (transports, application_server_key) = push_transports(config)?;
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
            // What the configured transports will actually carry, rather than a
            // constant chosen when none of them existed: Web Push allows just over
            // 4 KiB (RFC 8291), and a deployment with no transport allows nothing,
            // so every message it produces is a poke.
            transports.max_payload_bytes(),
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

            application_server_key,

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

/// The push transports this deployment configured, and the key a browser needs to
/// subscribe to the first of them (design §6.2, §11).
///
/// The key is returned rather than read back off the registry because the registry
/// holds `dyn PushTransport`, which is the right shape for the fan-out — it delivers
/// and says nothing else — and the wrong one for a value only Web Push has.
///
/// Every failure here is an operator's, and every one of them is answered at startup
/// rather than at the first push: a key file that cannot be read, a key that is not
/// P-256, a contact no relay would accept. Finding out because somebody's phone
/// stayed silent is the failure this refuses.
fn push_transports(config: &Config) -> anyhow::Result<(TransportRegistry, Option<String>)> {
    let mut registry = TransportRegistry::empty();

    let Some(settings) = &config.push.webpush else {
        return Ok((registry, None));
    };

    // The key is a path in the configuration and not the key itself (§11), so it can
    // be mounted from whatever the cluster keeps secrets in without passing through
    // a file that gets committed by accident.
    let pem = std::fs::read_to_string(&settings.vapid_key_file).map_err(|error| {
        anyhow::anyhow!(
            "[push.webpush] vapid_key_file `{}` cannot be read: {error}",
            settings.vapid_key_file.display()
        )
    })?;

    let webpush = WebPush::new(&WebPushConfig {
        private_key_pem: pem,
        contact: settings.contact.clone(),
    })?;

    let key = webpush.public_key().to_owned();
    registry = registry.with(Arc::new(webpush));

    Ok((registry, Some(key)))
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    /// Write a key somewhere only this test will look.
    fn key_file(test: &str, contents: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("wnk-vapid-{}-{test}.pem", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn configured(vapid_key_file: std::path::PathBuf) -> Config {
        Config {
            push: crate::config::Push {
                webpush: Some(crate::config::WebPush {
                    vapid_key_file,
                    contact: "mailto:ops@weebo.poc".to_owned(),
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_deployment_with_no_push_section_gets_the_empty_registry() {
        let (registry, key) = push_transports(&Config::default()).unwrap();

        assert!(registry.names().is_empty());
        assert_eq!(
            registry.max_payload_bytes(),
            0,
            "with no transport there is no payload that fits"
        );
        assert!(
            key.is_none(),
            "`GET /push/webpush` answers 501 rather than handing out a key that is not there"
        );
    }

    #[test]
    fn a_configured_key_registers_the_transport_and_yields_what_a_browser_subscribes_with() {
        let config = configured(key_file("ok", &notify_push_webpush::test_vapid_key()));

        let (registry, key) = push_transports(&config).unwrap();

        assert_eq!(registry.names(), vec!["webpush"]);
        assert!(
            registry.max_payload_bytes() > 3_000,
            "the fan-out's payload budget now comes from the transport itself"
        );
        assert_eq!(
            key.unwrap().len(),
            87,
            "65 octets of uncompressed point, base64url"
        );
    }

    #[test]
    fn a_key_file_that_is_not_there_stops_the_process_instead_of_starting_mute() {
        let config = configured("/nowhere/vapid.pem".into());

        let error = push_transports(&config).unwrap_err().to_string();

        assert!(
            error.contains("vapid_key_file"),
            "an operator has to be told which key, not that push failed: {error}"
        );
    }

    #[test]
    fn a_key_file_that_is_not_a_key_is_refused_at_startup_and_not_at_the_first_push() {
        let config = configured(key_file("garbage", "not a key at all\n"));

        assert!(push_transports(&config).is_err());
    }
}
