//! Building the application from the configuration (design §4): every adapter is built once and
//! handed out as `Arc<dyn Port>`. One `PgStore` serves eleven ports to share transactions (ADR-0012).

use std::sync::Arc;

use notify_app::{
    Attribution, Authenticate, Authorizer, BrowseAudit, ChannelInventory, CloseStale, CountUnread,
    CreateApiKey, CreateChannel, CreateServiceAccount, DeactivateServiceAccount, DeleteChannel,
    DeleteNotification, DrainFanout, DrainSinks, EnrolDevice, ExchangeToken, Fanout,
    GetNotification, ListApiKeys, ListChannels, ListDeadLetters, ListDevices, ListInbox,
    ListInteractions, ListRevisions, ListServiceAccounts, ListTrustPolicies, ManageMembers,
    OsRngKeyGenerator, PostReceipt, PresignAttachment, PublishNotification, PurgeExpired,
    ReadAvatar, ReadChannel, ReadDnd, ReadProfile, ReadReceipts, ReadReconciliation,
    ReconciliationLog, RecordInteraction, RequeueDeadLetter, ResolveStreamStart, RevokeApiKey,
    RevokeDevice, SearchNotifications, SelfJoin, SendDndDigests, SetApiKeyLimit, SetAvatar, SetDnd,
    SilenceNotification, SyncBacklog, UpdateChannel, UpdateProfile, UpsertNotification,
};
use notify_attachments_fs::{FsAttachments, FsConfig};
use notify_attachments_s3::{S3Attachments, S3Config};
use notify_avatars::{ImageAttachmentThumbnailer, ImageAvatarProcessor};
use notify_domain::{Pattern, TrustPolicies, TrustPolicy as DomainTrustPolicy};
use notify_eventbus_pg::PgEventBus;
use notify_http_actix::ApiState;
use notify_metrics_prometheus::PrometheusMetrics;
use notify_oidc::{
    Argon2SecretHasher, FederationTokens, GuardedSecretHasher, OidcConfig, OidcVerifier,
};
use notify_persistence_pg::{PgLeaderElection, PgStore};
use notify_ports::{AttachmentStore, AttachmentThumbnailer, EventBus, SystemClock};
use notify_push_apns::{Apns, ApnsConfig};
use notify_push_unifiedpush::UnifiedPush;
use notify_push_webpush::{WebPush, WebPushConfig};
use notify_sinks_email::{EmailSink, SmtpSettings};
use notify_sinks_webhook::WebhookSink;

use crate::bus::SharedBus;
use crate::config::{AttachmentsBackend, Config};
use crate::push::TransportRegistry;
use crate::reconcile::Reconciler;
use crate::sinks::{SinkFleet, SinkRoutes};

/// Everything the process needs, built once.
pub(crate) struct Application {
    /// What the HTTP layer is handed.
    pub state: ApiState,
    /// The store, for `/readyz` and for the migrations.
    pub store: PgStore,
    /// The task holding this replica's `LISTEN` connection; abort it before the runtime ends.
    pub pump: actix_web::rt::task::JoinHandle<()>,
    /// The live feed's reader (`notify_app::LiveFeed`); abort it with the pump.
    pub feed: actix_web::rt::task::JoinHandle<()>,
    /// The poke bus, for the watermark watcher's re-pokes.
    pub pokes: Arc<dyn EventBus>,
    /// The TTL purge, run on a timer by whichever replica leads.
    pub purge: PurgeExpired,
    /// The stale-closure sweep (§7.2bis), leader-run before the purge since closing starts retention.
    pub close_stale: CloseStale,
    /// The do-not-disturb digest (§7.4), on the same terms.
    pub digests: SendDndDigests,
    /// The outbound sink drain (§7.1bis), on the same terms again.
    pub drain: DrainSinks,
    /// The feed poll (RFC 0002), on the same terms again.
    pub poll_feeds: notify_app::PollFeeds,
    /// What `[[channels.managed.feeds]]` declared; swapped on `SIGHUP`.
    pub feed_routes: crate::feeds::FeedRoutes,
    /// What `[[transforms]]` declared (RFC 0003); swapped on `SIGHUP`.
    pub transform_routes: crate::transforms::TransformRoutes,
    /// The fan-out outbox worker (ADR-0025): on every replica, not just the leader.
    pub fanout_drain: DrainFanout,
    /// What `[[channels.managed.sinks]]` configured; shared with the fan-out and drain, swapped on `SIGHUP`.
    pub routes: SinkRoutes,
    /// The adapters behind those routes, swapped alongside them.
    pub fleet: SinkFleet,
    /// Whether the `LISTEN` pump is subscribed, for `/readyz`.
    pub bus: crate::bus::BusHealth,
    /// GitOps reconciliation.
    pub reconciler: Reconciler,
    /// The trust policies the token exchange consults, swappable on `SIGHUP` (§5.3, §11).
    pub policies: notify_app::LivePolicies,
    /// Where each pass is left for `GET /admin/reconciliation` (§10.4bis); the run loop writes it.
    pub reconciliation_log: ReconciliationLog,
    /// The push transports this deployment configured, for `/readyz` and the
    /// startup log.
    pub transports: TransportRegistry,
    /// The registry `GET /metrics` serves (design §13).
    pub metrics: Arc<dyn notify_ports::Metrics>,
}

impl core::fmt::Debug for Application {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Application")
            .field("transports", &self.transports)
            .finish_non_exhaustive()
    }
}

impl Application {
    /// Fails if the database cannot be reached or the issuer does not parse.
    #[expect(
        clippy::too_many_lines,
        reason = "one statement per use case; splitting it would hide which port \
                  each one was handed, which is the only thing this function says"
    )]
    pub(crate) async fn build(config: &Config) -> anyhow::Result<Self> {
        let store = PgStore::connect_with(
            config.database.url.expose(),
            &crate::config::pool(&config.database),
        )
        .await?;
        // One LISTEN per replica, fanned out in process (see `bus`).
        let (shared, pump) =
            SharedBus::start(Arc::new(PgEventBus::new(store.pool().clone()))).await?;
        let bus_health = shared.health();
        let bus: Arc<dyn EventBus> = Arc::new(shared);

        let clock = Arc::new(SystemClock);
        let repository = Arc::new(store.clone());
        // One read of the log per wake, for every stream this replica holds.
        let live = notify_app::LiveFeed::new(repository.clone());
        let feed = actix_web::rt::spawn(live.clone().run(bus.subscribe().await?));

        // `audiences` always includes the client id; extras are for issuers naming the API (§5.1).
        let mut oidc = OidcConfig::new(config.oidc.issuer.parse()?, config.oidc.client_id.clone());
        oidc.audiences.extend(config.oidc.audiences.iter().cloned());
        oidc.groups_claim.clone_from(&config.oidc.groups_claim);
        oidc.trust_unverified_email = config.oidc.trust_unverified_email;
        oidc.stale_key_grace = config.oidc.jwks_stale_grace.as_duration().try_into()?;
        let verifier = Arc::new(OidcVerifier::new(oidc, clock.clone())?);
        // Argon2id behind memoisation and admission control (`notify_oidc::guard`).
        let hasher = Arc::new(GuardedSecretHasher::new(Argon2SecretHasher));

        // One metrics registry per process (§13).
        let metrics: Arc<dyn notify_ports::Metrics> = Arc::new(PrometheusMetrics::new());

        // Missing transports are named by the registry; the fan-out carries on (§14.3).
        let (transports, application_server_key) = push_transports(config)?;
        let transports = transports.measured_by(metrics.clone());
        let dispatcher = Arc::new(transports.clone());

        // No object store means a refusing double (§14.3). `attachments_fs` is kept concrete for its
        // `verify`, used by the filesystem routes (ADR-0022).
        let mut attachments_fs: Option<Arc<FsAttachments>> = None;
        let attachments: Arc<dyn AttachmentStore> = match &config.attachments {
            Some(settings) => match settings.kind {
                AttachmentsBackend::S3 => Arc::new(S3Attachments::connect(&S3Config {
                    endpoint: settings.endpoint.parse()?,
                    bucket: settings.bucket.clone(),
                    region: settings.region.clone(),
                    access_key: settings.access_key.expose().to_owned(),
                    secret_key: settings.secret_key.expose().to_owned(),
                })?),
                AttachmentsBackend::Filesystem => {
                    let store = Arc::new(FsAttachments::connect(&FsConfig {
                        root: settings.path.clone().into(),
                        public_base_url: settings.public_base_url.parse()?,
                        token_secret: settings.token_secret.expose().to_owned(),
                    })?);
                    attachments_fs = Some(store.clone());
                    store
                }
            },
            None => Arc::new(NoAttachments),
        };
        let avatars = Arc::new(ImageAvatarProcessor::new());
        let thumbnailer: Arc<dyn AttachmentThumbnailer> =
            Arc::new(ImageAttachmentThumbnailer::new());

        // Sinks (§7.1bis), built before the fan-out, which takes the routing table.
        let routes = SinkRoutes::build(config)?;
        let sinks = sink_fleet(config, &routes)?;

        let authorizer = Authorizer::new(repository.clone());
        let fanout = Fanout::new(
            bus.clone(),
            repository.clone(),
            dispatcher,
            repository.clone(),
            repository.clone(),
            clock.clone(),
            // Upper bound over the configured transports; each still checks its own limit and pokes
            // when over. None means every message is a poke.
            transports.max_payload_bytes(),
        );

        // Always handed the table, even empty: it is swapped on `SIGHUP`, so a reload can add a sink.
        let fanout = fanout.with_sinks(Arc::new(routes.clone()));
        // Unconditional: with no store, `NoAttachments::get` answers `None`, so nothing is derived.
        let fanout = fanout.with_thumbnails(attachments.clone(), thumbnailer.clone());
        // Pushes and thumbnails for committed publishes (ADR-0025). The same
        // `fanout` the publish use cases get, so a publish wakes this replica's
        // worker at once.
        let fanout_drain = DrainFanout::new(repository.clone(), repository.clone(), fanout.clone());

        // Shared by everything that has to answer "is this notification still
        // there, and may this caller see it".
        let get = GetNotification::new(repository.clone(), authorizer.clone(), clock.clone());

        // Shared by every writer that shows an author (§2.4).
        let attribution = Attribution::new(repository.clone(), repository.clone());

        // Federation (§5.3); `None` makes the exchange answer 501.
        let federation = federation_tokens(config, clock.clone())?;
        // The CLI download (§9); `None` makes `GET /cli` answer 501.
        let (cli_binary, cli_digest) = match cli_binary(config)? {
            Some((bytes, digest)) => (Some(bytes), Some(digest)),
            None => (None, None),
        };
        // One shared set, swapped on `SIGHUP`, so the console shows what is enforced.
        let policies = notify_app::LivePolicies::new(trust_policies(config)?);
        let console_policies = policies.clone();
        // Written by the reconcile loop, read by the console.
        let reconciliation_log = ReconciliationLog::new();
        let audience = config
            .federation
            .as_ref()
            .and_then(|settings| settings.audience.clone())
            .unwrap_or_else(|| "weebo-notify".to_owned());

        // Unique per process: pods often all run as PID 1, so the hostname and a timestamp join it.
        let replica = format!(
            "{}-{}-{}",
            std::env::var("HOSTNAME").unwrap_or_default(),
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        );
        // Built before the state: the JSON webhook route reads them (RFC 0003).
        let transform_routes = crate::transforms::TransformRoutes::build(
            config,
            notify_transform_cel::Slots::per_core(),
        )?;
        transform_routes.report(metrics.as_ref());
        let upsert = UpsertNotification::new(
            repository.clone(),
            repository.clone(),
            repository.clone(),
            repository.clone(),
            clock.clone(),
            authorizer.clone(),
            fanout.clone(),
            attribution.clone(),
        );
        let state = ApiState {
            live,
            metrics: metrics.clone(),
            streams: Arc::new(notify_http_actix::StreamSlots::clustered(
                Arc::new(store.clone()),
                replica,
            )),
            authenticate: {
                let base = Authenticate::new(
                    verifier,
                    repository.clone(),
                    repository.clone(),
                    repository.clone(),
                    repository.clone(),
                    hasher.clone(),
                    clock.clone(),
                )
                // GitOps only (§5.4); empty means no platform admins.
                .with_platform_admins(config.admin.oidc_groups.clone())
                .with_group_snapshot_ttl(config.auth.group_snapshot_ttl.as_duration().try_into()?);

                match federation.clone() {
                    Some(tokens) => base.with_federation(tokens),
                    None => base,
                }
            },

            publish: PublishNotification::new(
                repository.clone(),
                repository.clone(),
                repository.clone(),
                repository.clone(),
                clock.clone(),
                authorizer.clone(),
                fanout.clone(),
                attribution.clone(),
            ),
            upsert: upsert.clone(),
            ingest_json: notify_app::IngestJson::new(
                authorizer.clone(),
                upsert,
                Arc::new(transform_routes.clone()),
                metrics.clone(),
            ),
            delete_notification: DeleteNotification::new(
                repository.clone(),
                authorizer.clone(),
                clock.clone(),
                fanout.clone(),
            ),
            close_notification: notify_app::CloseNotification::new(
                repository.clone(),
                authorizer.clone(),
                clock.clone(),
                fanout.clone(),
                attribution.clone(),
                metrics.clone(),
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
            unread: CountUnread::new(repository.clone(), authorizer.clone()),

            record_interaction: RecordInteraction::new(
                repository.clone(),
                get.clone(),
                clock.clone(),
                fanout.clone(),
                attribution.clone(),
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
                get.clone(),
                clock.clone(),
                fanout.clone(),
            ),
            readership: ReadReceipts::new(
                repository.clone(),
                repository.clone(),
                repository.clone(),
                get,
            ),
            exchange: federation.map(|tokens| {
                ExchangeToken::new(
                    tokens,
                    repository.clone(),
                    repository.clone(),
                    repository.clone(),
                    clock.clone(),
                    policies.clone(),
                    audience,
                )
            }),

            audit: BrowseAudit::new(repository.clone()),
            inventory: ChannelInventory::new(repository.clone()),
            self_join: SelfJoin::new(repository.clone(), repository.clone(), clock.clone())
                .with_bus(bus.clone()),
            trust_policies: ListTrustPolicies::new(console_policies),
            reconciliation: ReadReconciliation::new(reconciliation_log.clone()),
            dlq: ListDeadLetters::new(repository.clone()),
            requeue: RequeueDeadLetter::new(repository.clone(), repository.clone(), clock.clone()),
            presign: PresignAttachment::new(
                notify_app::Attachments {
                    store: attachments.clone(),
                    registry: repository.clone(),
                },
                authorizer.clone(),
                clock.clone(),
            ),
            attachment: notify_app::ReadAttachment::new(
                notify_app::Attachments {
                    store: attachments.clone(),
                    registry: repository.clone(),
                },
                repository.clone(),
                authorizer.clone(),
            ),
            attachments_fs: attachments_fs.clone(),
            attachment_thumbnail: notify_app::ReadAttachmentThumbnail::new(
                notify_app::Attachments {
                    store: attachments.clone(),
                    registry: repository.clone(),
                },
                repository.clone(),
                authorizer.clone(),
            ),

            sync: SyncBacklog::new(
                repository.clone(),
                repository.clone(),
                repository.clone(),
                authorizer.clone(),
                clock.clone(),
            ),
            stream_start: ResolveStreamStart::new(repository.clone(), repository.clone()),

            channels: ListChannels::new(repository.clone(), authorizer.clone()),
            read_channel: ReadChannel::new(authorizer.clone()),
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
            )
            .with_bus(bus.clone()),

            create_key: CreateApiKey::new(
                repository.clone(),
                repository.clone(),
                hasher,
                Arc::new(OsRngKeyGenerator),
                authorizer.clone(),
                repository.clone(),
                clock.clone(),
            ),
            keys: ListApiKeys::new(repository.clone(), repository.clone(), authorizer.clone()),
            revoke_key: RevokeApiKey::new(
                repository.clone(),
                repository.clone(),
                authorizer.clone(),
                repository.clone(),
                clock.clone(),
            ),
            set_key_limit: SetApiKeyLimit::new(
                repository.clone(),
                repository.clone(),
                clock.clone(),
            ),

            enrol_device: EnrolDevice::new(repository.clone(), repository.clone(), clock.clone())
                .with_max_per_user(config.devices.max_per_user),
            devices: ListDevices::new(repository.clone()),
            revoke_device: RevokeDevice::new(repository.clone(), repository.clone(), clock.clone()),

            profile: ReadProfile::new(repository.clone()),
            update_profile: UpdateProfile::new(repository.clone()),
            set_avatar: SetAvatar::new(repository.clone(), avatars, attachments.clone()),
            avatar: ReadAvatar::new(repository.clone(), attachments.clone()),

            dnd: ReadDnd::new(repository.clone()),
            set_dnd: SetDnd::new(repository.clone(), clock.clone(), fanout.clone()),

            application_server_key,
            client_config: client_config(config),

            accounts: ListServiceAccounts::new(repository.clone()),
            users: notify_app::SearchUsers::new(repository.clone()),
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

            cli_binary,
            cli_digest,
        };

        let purge = PurgeExpired::new(
            repository.clone(),
            repository.clone(),
            repository.clone(),
            // The store the presign writes into (§6.1); the registry is the database (D25).
            notify_app::Attachments {
                store: attachments.clone(),
                registry: repository.clone(),
            },
            Arc::new(PgLeaderElection::new(store.pool().clone())),
            bus.clone(),
            clock.clone(),
        )
        .with_limits(notify_app::PurgeLimits {
            batch: config.retention.batch_size,
            budget: config.retention.pass_budget.as_duration(),
        });
        // Opt-in (ADR-0027): without `[retention] audit` the log keeps everything.
        let purge = match config.retention.audit {
            Some(keep) => purge.with_audit_retention(repository.clone(), keep.as_duration()),
            None => purge,
        };

        // Its own leader lock, so a slow purge cannot stall closures (see `CloseStale::JOB`).
        let close_stale = CloseStale::new(
            repository.clone(),
            repository.clone(),
            repository.clone(),
            Arc::new(PgLeaderElection::new(store.pool().clone())),
            bus.clone(),
            clock.clone(),
        )
        .with_metrics(metrics.clone());

        // The §7.4 digest, leader-only so each push is sent once.
        let digests = SendDndDigests::new(
            repository.clone(),
            repository.clone(),
            Arc::new(PgLeaderElection::new(store.pool().clone())),
            fanout,
            clock.clone(),
        );

        // The sink drain (§7.1bis), leader-only so each webhook is delivered once.
        let drain = DrainSinks::new(
            repository.clone(),
            Arc::new(routes.clone()),
            Arc::new(sinks.clone()),
            Arc::new(PgLeaderElection::new(store.pool().clone())),
            clock.clone(),
        )
        .with_dead_letter_retention(
            time::Duration::try_from(config.sinks.dead_letter_retention.as_duration())
                .unwrap_or(time::Duration::MAX),
        );

        // The feed poll (RFC 0002), leader-only so each item is published once; it
        // publishes through the API's own upsert, as the feed's service account.
        let feed_routes = crate::feeds::FeedRoutes::build(config)?;
        let poll_feeds = notify_app::PollFeeds::new(
            Arc::new(feed_routes.clone()),
            repository.clone(),
            Arc::new(notify_feeds_http::HttpFeeds::new()?),
            state.upsert.clone(),
            Arc::new(PgLeaderElection::new(store.pool().clone())),
            clock.clone(),
            metrics.clone(),
        );

        let reconciler = Reconciler::new(
            repository.clone(),
            repository.clone(),
            repository.clone(),
            clock,
            repository.clone(),
            repository.clone(),
            repository,
        )
        .with_metrics(metrics.clone());

        Ok(Self {
            state,
            store,
            pump,
            feed,
            pokes: bus,
            purge,
            close_stale,
            digests,
            drain,
            poll_feeds,
            feed_routes,
            transform_routes,
            fanout_drain,
            routes,
            fleet: sinks,
            bus: bus_health,
            reconciler,
            policies,
            reconciliation_log,
            transports,
            metrics,
        })
    }
}

/// The sink adapters the channels actually declare; misconfiguration fails at startup.
pub(crate) fn sink_fleet(config: &Config, routes: &SinkRoutes) -> anyhow::Result<SinkFleet> {
    let mut fleet = SinkFleet::empty();
    let kinds = routes.kinds();

    if kinds.contains(&"webhook") {
        let webhook: Arc<dyn notify_ports::SinkDispatcher> = Arc::new(WebhookSink::new()?);
        fleet = fleet.with(&webhook);
    }

    if kinds.contains(&"email") {
        let Some(smtp) = &config.sinks.smtp else {
            anyhow::bail!("an email sink is configured with no [sinks.smtp] relay");
        };

        let password = smtp
            .password_ref
            .as_deref()
            .map(|reference| crate::config::resolve_secret("[sinks.smtp] password_ref", reference))
            .transpose()?;

        let mail: Arc<dyn notify_ports::SinkDispatcher> =
            Arc::new(EmailSink::new(&SmtpSettings {
                url: smtp.url.parse()?,
                from: smtp.from.clone(),
                username: smtp.username.clone(),
                password,
                permalink_base: smtp.permalink_base.as_deref().map(str::parse).transpose()?,
            })?);
        fleet = fleet.with(&mail);
    }

    Ok(fleet)
}

/// The push transports configured, and the Web Push key a browser subscribes with (design §6.2, §11).
/// Every operator mistake fails here at startup rather than at the first push.
fn push_transports(config: &Config) -> anyhow::Result<(TransportRegistry, Option<String>)> {
    let mut registry = TransportRegistry::empty();
    let mut key = None;

    if let Some(settings) = &config.push.webpush {
        // A path, not the key, so it can be mounted from a secret store (§11).
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

        key = Some(webpush.public_key().to_owned());
        registry = registry.with(Arc::new(webpush));
    }

    if config.push.unifiedpush.is_some() {
        // No credential to read; only the HTTP client can fail.
        registry = registry.with(Arc::new(UnifiedPush::new()?));
    }

    if let Some(settings) = &config.push.apns {
        // A path, not the key, as with `vapid_key_file`.
        let pem = std::fs::read_to_string(&settings.key_file).map_err(|error| {
            anyhow::anyhow!(
                "[push.apns] key_file `{}` cannot be read: {error}",
                settings.key_file.display()
            )
        })?;

        let apns = Apns::new(&ApnsConfig {
            key_pem: pem,
            key_id: settings.key_id.clone(),
            team_id: settings.team_id.clone(),
            topic: settings.topic.clone(),
            sandbox: settings.sandbox,
        })?;

        registry = registry.with(Arc::new(apns));
    }

    Ok((registry, key))
}

/// The federation adapter, when `[federation]` says there is one (`None` makes the exchange 501).
/// An unusable key is an error at startup. The issuer allowlist is the trust policies'
/// (a security boundary, see [`notify_oidc::FederationTokens`]).
fn federation_tokens(
    config: &Config,
    clock: Arc<SystemClock>,
) -> anyhow::Result<Option<Arc<FederationTokens>>> {
    let Some(settings) = &config.federation else {
        return Ok(None);
    };

    let pem = std::fs::read(&settings.signing_key_file).map_err(|error| {
        anyhow::anyhow!(
            "[federation] signing_key_file `{}` cannot be read: {error}",
            settings.signing_key_file.display()
        )
    })?;

    let issuers = config.federation_issuers();

    Ok(Some(Arc::new(FederationTokens::new(
        &pem,
        &issuers,
        settings
            .audience
            .clone()
            .unwrap_or_else(|| "weebo-notify".to_owned()),
        clock,
    )?)))
}

/// The CLI binary this replica serves, when `[cli]` says there is one (design §9); `None` → `501`.
/// Read once into `Bytes`, so each download is a refcount bump; an unreadable path fails startup.
fn cli_binary(config: &Config) -> anyhow::Result<Option<(actix_web::web::Bytes, String)>> {
    let Some(settings) = &config.cli else {
        return Ok(None);
    };

    let bytes = std::fs::read(&settings.binary_path).map_err(|error| {
        anyhow::anyhow!(
            "[cli] binary_path `{}` cannot be read: {error}",
            settings.binary_path.display()
        )
    })?;

    // Served as `X-Content-Digest` for checking against the release (SEC-P2-26/27).
    let digest = {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        base16ct::lower::encode_string(&hasher.finalize())
    };
    tracing::info!(
        path = %settings.binary_path.display(),
        sha256 = %digest,
        "serving the CLI binary at GET /api/v1/cli"
    );

    Ok(Some((actix_web::web::Bytes::from(bytes), digest)))
}

/// The trust policies, global and channel-scoped (marked `channel_scoped` for §11's precedence).
pub(crate) fn trust_policies(config: &Config) -> anyhow::Result<TrustPolicies> {
    let mut policies = Vec::new();

    for policy in &config.trust_policy {
        policies.push(DomainTrustPolicy {
            issuer: policy.issuer.clone(),
            subject: Pattern::parse(&policy.subject)?,
            channels: policy
                .channels
                .iter()
                .map(|one| Pattern::parse(one))
                .collect::<Result<_, _>>()?,
            scopes: policy.scopes.clone(),
            channel_scoped: false,
        });
    }

    for channel in &config.channels.managed {
        for publisher in &channel.publishers {
            policies.push(DomainTrustPolicy {
                issuer: publisher.issuer.clone(),
                subject: Pattern::parse(&publisher.subject)?,
                // Only its own channel, or it would be a global policy in disguise.
                channels: vec![Pattern::parse(&channel.id)?],
                scopes: publisher.scopes.clone(),
                channel_scoped: true,
            });
        }
    }

    Ok(TrustPolicies::new(policies))
}

/// The object store a deployment without one gets: refuses with `NotConfigured` (§14.3).
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
        // Nothing to delete is the desired state.
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

    async fn put_thumbnail(
        &self,
        _id: notify_domain::AttachmentId,
        _content_type: &str,
        _bytes: Vec<u8>,
    ) -> notify_ports::PortResult<()> {
        Err(unconfigured())
    }

    async fn get_thumbnail(
        &self,
        _id: notify_domain::AttachmentId,
    ) -> notify_ports::PortResult<Option<notify_ports::StoredObject>> {
        // `None`, like `get`, so nothing is derived.
        Ok(None)
    }

    async fn health(&self) -> notify_ports::PortResult<()> {
        // Healthy: no bucket configured is not a broken instance.
        Ok(())
    }
}

const fn unconfigured() -> notify_ports::PortError {
    notify_ports::PortError::NotConfigured {
        what: "attachments",
    }
}

/// What `GET /client-config` publishes (RFC 0001): values already public in the web
/// image's `env.js`, read from the same config the server validates tokens with.
fn client_config(config: &Config) -> notify_http_actix::dto::ClientConfig {
    let attachment_origin = config.attachments.as_ref().and_then(|attachments| {
        let base = match attachments.kind {
            AttachmentsBackend::S3 => &attachments.endpoint,
            AttachmentsBackend::Filesystem => &attachments.public_base_url,
        };
        url::Url::parse(base)
            .ok()
            .map(|url| url.origin().ascii_serialization())
    });
    notify_http_actix::dto::ClientConfig {
        oidc_issuer: config.oidc.issuer.clone(),
        oidc_client_id: config.oidc.client_id.clone(),
        attachment_origin,
    }
}

#[cfg(test)]
mod tests {
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

        assert_eq!(registry.names(), [] as [&str; 0]);
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
    fn an_empty_unifiedpush_section_registers_the_transport_with_no_key_to_hand_out() {
        let config = Config {
            push: crate::config::Push {
                unifiedpush: Some(crate::config::UnifiedPush {}),
                ..Default::default()
            },
            ..Default::default()
        };

        let (registry, key) = push_transports(&config).unwrap();

        assert_eq!(registry.names(), vec!["unifiedpush"]);
        assert!(
            key.is_none(),
            "UnifiedPush identifies no one to a relay the way VAPID does — there is \
             nothing here for a device to subscribe with"
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

    #[test]
    fn a_configured_apns_key_registers_the_transport() {
        let config = Config {
            push: crate::config::Push {
                apns: Some(crate::config::Apns {
                    key_file: key_file("apns-ok", &notify_push_apns::test_apns_key()),
                    key_id: "ABCDE12345".to_owned(),
                    team_id: "TEAM123456".to_owned(),
                    topic: "poc.weebo.notify".to_owned(),
                    sandbox: false,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let (registry, key) = push_transports(&config).unwrap();

        assert_eq!(registry.names(), vec!["apns"]);
        assert!(
            key.is_none(),
            "the VAPID-style key a browser subscribes with is a Web Push concept only"
        );
    }

    #[test]
    fn an_apns_key_file_that_is_not_there_stops_the_process_instead_of_starting_mute() {
        let config = Config {
            push: crate::config::Push {
                apns: Some(crate::config::Apns {
                    key_file: "/nowhere/apns.p8".into(),
                    key_id: "ABCDE12345".to_owned(),
                    team_id: "TEAM123456".to_owned(),
                    topic: "poc.weebo.notify".to_owned(),
                    sandbox: false,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let error = push_transports(&config).unwrap_err().to_string();

        assert!(
            error.contains("key_file"),
            "an operator has to be told which key, not that push failed: {error}"
        );
    }

    #[test]
    fn a_deployment_with_no_cli_section_serves_nothing() {
        let served = cli_binary(&Config::default()).unwrap();

        assert!(
            served.is_none(),
            "`GET /cli` answers 501 rather than guessing where a binary might be"
        );
    }

    #[test]
    fn a_configured_binary_path_is_read_once_at_boot() {
        let path = std::env::temp_dir().join(format!("wnk-cli-{}.bin", std::process::id()));
        std::fs::write(&path, b"not really a binary, but bytes").unwrap();
        let config = Config {
            cli: Some(crate::config::Cli { binary_path: path }),
            ..Default::default()
        };

        let (bytes, digest) = cli_binary(&config).unwrap().unwrap();

        assert_eq!(bytes.as_ref(), b"not really a binary, but bytes");
        // Served as `X-Content-Digest` (SEC-P2-26/27).
        assert_eq!(
            digest, "181dcdff3eebcb931b129e19f67aecbbaff4816770bd0c1da0d2af3b4b60a113",
            "the digest is not SHA-256 of the bytes actually read"
        );
    }

    #[test]
    fn a_cli_binary_path_that_is_not_there_stops_the_process_instead_of_starting_with_nothing_to_serve()
     {
        let config = Config {
            cli: Some(crate::config::Cli {
                binary_path: "/nowhere/notify".into(),
            }),
            ..Default::default()
        };

        let error = cli_binary(&config).unwrap_err().to_string();

        assert!(
            error.contains("binary_path"),
            "an operator has to be told which path, not merely that the download failed: {error}"
        );
    }
}
