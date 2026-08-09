//! The HTTP surface, exercised end to end against the in-memory doubles.
//!
//! No container and no identity provider: the whole application is wired here the
//! way the composition root wires it, with `notify-ports/test-utils` standing in
//! for the adapters. What is under test is the adapter's own job — status codes,
//! problem bodies, query parsing, and the fact that nothing reaches a use case
//! without a caller.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failing assertion is a failing test"
)]

use std::sync::Arc;

use actix_web::{App, http::StatusCode, test, web};
use notify_app::{
    Attribution, Authenticate, Authorizer, BrowseAudit, ChannelInventory, CountUnread,
    CreateApiKey, CreateChannel, CreateServiceAccount, DeactivateServiceAccount, DeleteChannel,
    DeleteNotification, EnrolDevice, Fanout, GetNotification, ListApiKeys, ListChannels,
    ListDevices, ListInbox, ListInteractions, ListRevisions, ListServiceAccounts,
    ListTrustPolicies, ManageMembers, PostReceipt, PresignAttachment, PublishNotification,
    ReadAvatar, ReadChannel, ReadDnd, ReadProfile, ReadReceipts, ReadReconciliation,
    ReconciliationLog, RecordInteraction, ResolveStreamStart, RevokeApiKey, RevokeDevice,
    SearchNotifications, SelfJoin, SetApiKeyLimit, SetAvatar, SetDnd, SilenceNotification,
    SyncBacklog, UpdateChannel, UpdateProfile, UpsertNotification,
};
use notify_avatars::{ImageAttachmentThumbnailer, ImageAvatarProcessor};
use notify_domain::{Channel, ChannelId, Membership, Origin, Principal, Scope, User, UserId};
use notify_http_actix::ApiState;
use notify_ports::test_utils::{
    CountingKeyGenerator, InMemory, InMemoryAttachments, StaticTokenVerifier, StubSecretHasher,
    TestClock, TestRateLimiter,
};
use notify_ports::{ChannelRepository, Clock as _, UserRepository, VerifiedClaims};
use serde_json::{Value, json};
use time::Duration;
use url::Url;
use utoipa_actix_web::AppExt as _;

/// Everything a test needs to reach behind the HTTP layer.
struct Harness {
    store: InMemory,
    clock: TestClock,
    tokens: StaticTokenVerifier,
    limiter: TestRateLimiter,
    alice: UserId,
}

impl Harness {
    #[allow(
        clippy::too_many_lines,
        reason = "wiring, not logic: one line per use case, exactly as the composition \
                  root has to spell it out"
    )]
    fn new() -> (Self, ApiState) {
        let store = InMemory::new();
        let clock = TestClock::new();
        let tokens = StaticTokenVerifier::with_clock(Arc::new(clock.clone()));
        let limiter = TestRateLimiter::default();
        let attachments = InMemoryAttachments::new();
        let authorizer = Authorizer::new(Arc::new(store.clone()));

        let fanout = Fanout::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
            4_096,
        )
        .with_thumbnails(
            Arc::new(attachments.clone()),
            Arc::new(ImageAttachmentThumbnailer::new()),
        );
        let get = GetNotification::new(
            Arc::new(store.clone()),
            authorizer.clone(),
            Arc::new(clock.clone()),
        );

        // The same store answers for users and service accounts, so a publish made
        // through this harness carries the display name the profile holds (§2.4).
        let attribution = Attribution::new(Arc::new(store.clone()), Arc::new(store.clone()));

        let state = ApiState {
            bus: Arc::new(store.clone()),
            // Nothing here asserts on a number, so the harness measures nothing —
            // the exporter is exercised where it lives (`notify-metrics-prometheus`)
            // and the recording sites are two lines each.
            metrics: Arc::new(notify_ports::NoMetrics),
            // Real, unlike the metrics: this one refuses requests, so a harness
            // that stubbed it would test a server that has no ceiling.
            streams: Arc::new(notify_http_actix::StreamSlots::new()),
            authenticate: Authenticate::new(
                Arc::new(tokens.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(StubSecretHasher),
                Arc::new(clock.clone()),
            )
            // A fixed group name so `alice_as_platform_admin` below has something
            // to grant — no other test in this harness had ever needed the role.
            .with_platform_admins(vec!["platform-admins".to_owned()]),
            publish: PublishNotification::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(limiter.clone()),
                Arc::new(clock.clone()),
                authorizer.clone(),
                fanout.clone(),
                attribution.clone(),
            ),
            upsert: UpsertNotification::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(limiter.clone()),
                Arc::new(clock.clone()),
                authorizer.clone(),
                fanout.clone(),
                attribution.clone(),
            ),
            delete_notification: DeleteNotification::new(
                Arc::new(store.clone()),
                authorizer.clone(),
                Arc::new(clock.clone()),
                fanout.clone(),
            ),
            inbox: ListInbox::new(Arc::new(store.clone()), authorizer.clone()),
            notification: get.clone(),
            revisions: ListRevisions::new(Arc::new(store.clone()), get.clone()),
            search: SearchNotifications::new(
                Arc::new(store.clone()),
                authorizer.clone(),
                Arc::new(limiter.clone()),
                Arc::new(clock.clone()),
            ),
            unread: CountUnread::new(Arc::new(store.clone()), authorizer.clone()),
            record_interaction: RecordInteraction::new(
                Arc::new(store.clone()),
                get.clone(),
                Arc::new(clock.clone()),
                fanout.clone(),
                attribution.clone(),
            ),
            interactions: ListInteractions::new(Arc::new(store.clone()), get.clone()),
            silence: SilenceNotification::new(
                Arc::new(store.clone()),
                get.clone(),
                Arc::new(clock.clone()),
                fanout.clone(),
            ),
            receipt: PostReceipt::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                get.clone(),
                Arc::new(clock.clone()),
                fanout.clone(),
            ),
            readership: ReadReceipts::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                get,
            ),
            // No `[federation]` in the harness: the exchange answers 501, which is
            // what a deployment that does not federate is supposed to do.
            exchange: None,
            audit: BrowseAudit::new(Arc::new(store.clone())),
            inventory: ChannelInventory::new(Arc::new(store.clone())),
            self_join: SelfJoin::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            trust_policies: ListTrustPolicies::new(notify_app::LivePolicies::default()),
            // Never reconciled in the harness, which is a state the console has to
            // render: `null` and not zeroes (§10.4bis).
            reconciliation: ReadReconciliation::new(ReconciliationLog::new()),
            dlq: notify_app::ListDeadLetters::new(Arc::new(store.clone())),
            requeue: notify_app::RequeueDeadLetter::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            presign: PresignAttachment::new(
                notify_app::Attachments {
                    store: Arc::new(attachments.clone()),
                    registry: Arc::new(store.clone()),
                },
                authorizer.clone(),
                Arc::new(clock.clone()),
            ),
            attachment: notify_app::ReadAttachment::new(
                notify_app::Attachments {
                    store: Arc::new(attachments.clone()),
                    registry: Arc::new(store.clone()),
                },
                Arc::new(store.clone()),
                authorizer.clone(),
            ),
            attachment_thumbnail: notify_app::ReadAttachmentThumbnail::new(
                notify_app::Attachments {
                    store: Arc::new(attachments.clone()),
                    registry: Arc::new(store.clone()),
                },
                Arc::new(store.clone()),
                authorizer.clone(),
            ),
            sync: SyncBacklog::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                authorizer.clone(),
                Arc::new(clock.clone()),
            ),
            stream_start: ResolveStreamStart::new(Arc::new(store.clone()), Arc::new(store.clone())),
            channels: ListChannels::new(Arc::new(store.clone())),
            read_channel: ReadChannel::new(authorizer.clone()),
            create_channel: CreateChannel::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            update_channel: UpdateChannel::new(
                Arc::new(store.clone()),
                authorizer.clone(),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            delete_channel: DeleteChannel::new(
                Arc::new(store.clone()),
                authorizer.clone(),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            members: ManageMembers::new(
                Arc::new(store.clone()),
                authorizer.clone(),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            create_key: CreateApiKey::new(
                Arc::new(store.clone()),
                Arc::new(StubSecretHasher),
                Arc::new(CountingKeyGenerator::default()),
                authorizer,
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            keys: ListApiKeys::new(Arc::new(store.clone())),
            revoke_key: RevokeApiKey::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            set_key_limit: SetApiKeyLimit::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            enrol_device: EnrolDevice::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            devices: ListDevices::new(Arc::new(store.clone())),
            revoke_device: RevokeDevice::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),

            profile: ReadProfile::new(Arc::new(store.clone())),
            update_profile: UpdateProfile::new(Arc::new(store.clone())),
            set_avatar: SetAvatar::new(
                Arc::new(store.clone()),
                Arc::new(ImageAvatarProcessor::new()),
                Arc::new(attachments.clone()),
            ),
            avatar: ReadAvatar::new(Arc::new(attachments.clone())),

            dnd: ReadDnd::new(Arc::new(store.clone())),
            set_dnd: SetDnd::new(
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
                fanout.clone(),
            ),

            // A deployment that runs Web Push, so `GET /push/webpush` has something
            // to answer; the test for the other case builds its own state.
            application_server_key: Some(APPLICATION_SERVER_KEY.to_owned()),

            accounts: ListServiceAccounts::new(Arc::new(store.clone())),
            create_account: CreateServiceAccount::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            deactivate_account: DeactivateServiceAccount::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),

            // No `[cli]` in the harness: `GET /cli` answers 501, which is what a
            // deployment that does not serve the binary is supposed to do. The
            // test for the other case sets its own.
            cli_binary: None,
        };

        let harness = Self {
            store,
            clock,
            tokens,
            limiter,
            alice: UserId::generate(),
        };
        (harness, state)
    }

    /// Register alice, give her a token, and put her in a channel she can publish to.
    async fn with_alice_in(&self, channel: &str) -> ChannelId {
        // Through `from_claims` rather than a literal: it is the constructor the
        // login path uses, so the profile starts out following the provider exactly
        // as it would in production.
        let user = User::from_claims(
            self.alice,
            "alice",
            Some("alice@weebo.local"),
            Some("Alice Martin"),
            self.clock.now(),
        )
        .unwrap();
        UserRepository::upsert(&self.store, &user).await.unwrap();

        self.tokens.register(
            "alice-token",
            VerifiedClaims {
                issuer: Url::parse("https://idp.example").unwrap(),
                subject: "alice".to_owned(),
                email: Some("alice@weebo.local".to_owned()),
                display_name: Some("Alice Martin".to_owned()),
                groups: vec![],
                expires_at: self.clock.now() + Duration::hours(1),
            },
        );

        let id = ChannelId::parse(channel).unwrap();
        let channel = Channel {
            id: id.clone(),
            display_name: None,
            origin: Origin::Dynamic,
            policy: notify_domain::ChannelPolicy::default(),
            members: vec![Membership {
                principal: Principal::User { id: self.alice },
                scopes: vec![Scope::Publish, Scope::Subscribe, Scope::Manage],
            }],
            group_access: Vec::new(),
        };
        ChannelRepository::upsert(&self.store, &channel)
            .await
            .unwrap();

        id
    }

    /// A second token for the same alice, this one carrying the group
    /// `Authenticate` is wired to treat as platform-admin (§5.4, D24) — no other
    /// test in this harness had ever needed the role.
    fn alice_as_platform_admin(&self) {
        self.tokens.register(
            "alice-admin-token",
            VerifiedClaims {
                issuer: Url::parse("https://idp.example").unwrap(),
                subject: "alice".to_owned(),
                email: Some("alice@weebo.local".to_owned()),
                display_name: Some("Alice Martin".to_owned()),
                groups: vec!["platform-admins".to_owned()],
                expires_at: self.clock.now() + Duration::hours(1),
            },
        );
    }
}

/// Build the test server the same way the composition root builds the real one.
macro_rules! service {
    ($state:expr) => {{
        let (app, _) = App::new()
            .into_utoipa_app()
            .configure(notify_http_actix::configure)
            .split_for_parts();
        test::init_service(app.app_data(web::Data::new($state))).await
    }};
}

const AUTH: (&str, &str) = ("Authorization", "Bearer alice-token");
const ADMIN_AUTH: (&str, &str) = ("Authorization", "Bearer alice-admin-token");

/// A stand-in for what the composition root derives from the VAPID key. Not a real
/// point: nothing here decodes it, and the property under test is that it reaches a
/// browser unchanged.
const APPLICATION_SERVER_KEY: &str = "BEl62iUYgUivxIkv69yViEuiBIa-Ib9-SkFbx4gpXYm";

#[actix_web::test]
async fn nothing_is_reachable_without_a_credential() {
    let (_, state) = Harness::new();
    let app = service!(state);

    for (method, path) in [
        ("GET", "/api/v1/notifications"),
        ("POST", "/api/v1/notifications"),
        ("GET", "/api/v1/channels"),
        ("GET", "/api/v1/sync"),
        ("GET", "/api/v1/devices"),
        ("GET", "/api/v1/apikeys"),
        ("GET", "/api/v1/cli"),
    ] {
        let request = test::TestRequest::with_uri(path)
            .method(method.parse().unwrap())
            .set_json(json!({}))
            .to_request();
        let response = test::call_service(&app, request).await;

        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path} answered without a credential"
        );
        assert!(
            response.headers().contains_key("www-authenticate"),
            "{method} {path} returned a 401 that is not a challenge"
        );
    }
}

#[actix_web::test]
async fn publishing_returns_201_and_the_notification_lands_in_the_inbox() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .set_json(json!({
            "channel": "ci-infra",
            "title": "Pipeline infra #4521",
            "state": "open",
            "status": "info",
            "ttl": "48h",
        }))
        .to_request();
    let published: Value = test::call_and_read_body_json(&app, request).await;

    assert_eq!(published["notification"]["channel"], "ci-infra");
    assert_eq!(published["revision"]["seq"], 1);
    assert_eq!(published["replayed"], false);

    let request = test::TestRequest::get()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .to_request();
    let page: Value = test::call_and_read_body_json(&app, request).await;

    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        page["items"][0]["head"]["content"]["title"],
        "Pipeline infra #4521"
    );
}

#[actix_web::test]
async fn the_upsert_is_the_same_notification_twice_rather_than_two() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    for body in [
        json!({ "title": "Pipeline infra #4521", "state": "open" }),
        json!({ "state": "closed", "status": "success" }),
    ] {
        let request = test::TestRequest::put()
            .uri("/api/v1/channels/ci-infra/notifications/run-4521")
            .insert_header(AUTH)
            .set_json(body)
            .to_request();
        let response = test::call_service(&app, request).await;
        let status = response.status();
        let body = test::read_body(response).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    }

    let request = test::TestRequest::get()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .to_request();
    let page: Value = test::call_and_read_body_json(&app, request).await;

    assert_eq!(
        page["items"].as_array().unwrap().len(),
        1,
        "a retried CI job is one notification with two revisions"
    );
    assert_eq!(page["items"][0]["last_seq"], 2);
    assert_eq!(
        page["items"][0]["head"]["content"]["title"], "Pipeline infra #4521",
        "an absent field inherits from the head rather than being cleared"
    );
}

#[actix_web::test]
async fn an_idempotency_key_replays_rather_than_publishing_twice() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let body = json!({ "channel": "ci-infra", "title": "Deploy started" });
    let mut statuses = Vec::new();
    for _ in 0..2 {
        let request = test::TestRequest::post()
            .uri("/api/v1/notifications")
            .insert_header(AUTH)
            .insert_header(("Idempotency-Key", "deploy-77"))
            .set_json(&body)
            .to_request();
        statuses.push(test::call_service(&app, request).await.status());
    }

    assert_eq!(statuses[0], StatusCode::CREATED);
    assert_eq!(
        statuses[1],
        StatusCode::OK,
        "answering 201 twice would tell the client it published twice"
    );
}

#[actix_web::test]
async fn a_refusal_is_problem_json_carrying_the_code_clients_match_on() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .set_json(json!({ "channel": "ci-infra", "title": "   " }))
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/problem+json"
    );

    let problem: Value = test::read_body_json(response).await;
    assert_eq!(problem["code"], "field.empty");
    assert_eq!(problem["status"], 422);
}

#[actix_web::test]
async fn publishing_into_a_channel_you_are_not_in_says_nothing_about_whether_it_exists() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let mut answers = Vec::new();
    for channel in ["deploys", "does-not-exist"] {
        let request = test::TestRequest::post()
            .uri("/api/v1/notifications")
            .insert_header(AUTH)
            .set_json(json!({ "channel": channel, "title": "hello" }))
            .to_request();
        let response = test::call_service(&app, request).await;
        let status = response.status();
        let problem: Value = test::read_body_json(response).await;
        answers.push((status, problem["code"].clone()));
    }

    // A channel that exists but is not yours and one that does not exist have to be
    // indistinguishable, or the endpoint enumerates the instance.
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[0].0, StatusCode::FORBIDDEN);
    assert_eq!(answers[0].1, "scope.missing");
}

#[actix_web::test]
async fn the_channel_filter_narrows_and_cannot_widen() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;

    // A channel alice is not a member of, holding a notification.
    let other = ChannelId::parse("secrets").unwrap();
    ChannelRepository::upsert(
        &harness.store,
        &Channel {
            id: other,
            display_name: None,
            origin: Origin::Dynamic,
            policy: notify_domain::ChannelPolicy::default(),
            members: vec![],
            group_access: Vec::new(),
        },
    )
    .await
    .unwrap();

    let app = service!(state);
    let request = test::TestRequest::get()
        .uri("/api/v1/notifications?channels=secrets")
        .insert_header(AUTH)
        .to_request();
    let page: Value = test::call_and_read_body_json(&app, request).await;

    assert!(
        page["items"].as_array().unwrap().is_empty(),
        "?channels= must intersect with what the caller may read, not replace it"
    );
}

#[actix_web::test]
async fn a_query_value_that_is_not_one_of_ours_is_refused_before_any_work() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    for uri in [
        "/api/v1/notifications?state=finished",
        "/api/v1/notifications?min_priority=very-urgent",
        "/api/v1/notifications?cursor=not-a-cursor",
    ] {
        let request = test::TestRequest::get()
            .uri(uri)
            .insert_header(AUTH)
            .to_request();
        let response = test::call_service(&app, request).await;

        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{uri} was accepted"
        );
    }
}

#[actix_web::test]
async fn being_over_the_quota_is_a_429_that_says_when_to_come_back() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    harness
        .limiter
        .limit(notify_ports::Quota::PublishPerChannel {
            channel: "ci-infra".to_owned(),
        });
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .set_json(json!({ "channel": "ci-infra", "title": "too much" }))
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        response.headers().contains_key("retry-after"),
        "a 429 without Retry-After turns a polite client into a busy loop"
    );
}

#[actix_web::test]
async fn a_minted_key_shows_its_secret_once_and_the_listing_never_does() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/apikeys")
        .insert_header(AUTH)
        .set_json(json!({
            "owner": { "owner": "user", "id": harness.alice.to_string() },
            "label": "dotfiles",
            "scopes": ["publish"],
            "channels": ["ci-infra"],
        }))
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let minted: Value = test::read_body_json(response).await;
    let secret = minted["secret"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("wnk_"));

    let request = test::TestRequest::get()
        .uri("/api/v1/apikeys")
        .insert_header(AUTH)
        .to_request();
    let listed: Value = test::call_and_read_body_json(&app, request).await;

    let rendered = listed.to_string();
    assert!(
        !rendered.contains(&secret),
        "no endpoint may show a key secret a second time: {rendered}"
    );
}

#[actix_web::test]
async fn a_key_authenticates_the_same_endpoints_a_token_does() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/apikeys")
        .insert_header(AUTH)
        .set_json(json!({
            "owner": { "owner": "user", "id": harness.alice.to_string() },
            "scopes": ["publish"],
            "channels": ["ci-infra"],
        }))
        .to_request();
    let minted: Value = test::call_and_read_body_json(&app, request).await;
    let secret = minted["secret"].as_str().unwrap();

    let request = test::TestRequest::post()
        .uri("/api/v1/notifications")
        .insert_header(("Authorization", format!("Bearer {secret}")))
        .set_json(json!({ "channel": "ci-infra", "title": "from a key" }))
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::CREATED);
}

#[actix_web::test]
async fn a_platform_admin_narrows_a_keys_own_ceiling() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    harness.alice_as_platform_admin();
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/apikeys")
        .insert_header(AUTH)
        .set_json(json!({
            "owner": { "owner": "user", "id": harness.alice.to_string() },
            "scopes": ["publish"],
            "channels": ["ci-infra"],
        }))
        .to_request();
    let minted: Value = test::call_and_read_body_json(&app, request).await;
    let id = minted["id"].as_str().unwrap().to_owned();

    let request = test::TestRequest::patch()
        .uri(&format!("/api/v1/apikeys/{id}/limit"))
        .insert_header(ADMIN_AUTH)
        .set_json(json!({ "publish_per_minute": 5 }))
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::OK);

    let updated: Value = test::read_body_json(response).await;
    assert_eq!(updated["publish_per_minute"], 5);
}

#[actix_web::test]
async fn the_keys_own_owner_cannot_narrow_its_own_ceiling() {
    // D24, over the wire: a self-service ceiling would not be one.
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/apikeys")
        .insert_header(AUTH)
        .set_json(json!({
            "owner": { "owner": "user", "id": harness.alice.to_string() },
            "scopes": ["publish"],
            "channels": ["ci-infra"],
        }))
        .to_request();
    let minted: Value = test::call_and_read_body_json(&app, request).await;
    let id = minted["id"].as_str().unwrap().to_owned();

    let request = test::TestRequest::patch()
        .uri(&format!("/api/v1/apikeys/{id}/limit"))
        .insert_header(AUTH)
        .set_json(json!({ "publish_per_minute": 1 }))
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[actix_web::test]
async fn deleting_a_notification_twice_is_not_an_error_the_client_has_to_handle() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .set_json(json!({ "channel": "ci-infra", "title": "temporary" }))
        .to_request();
    let published: Value = test::call_and_read_body_json(&app, request).await;
    let id = published["notification"]["id"].as_str().unwrap();

    for _ in 0..2 {
        let request = test::TestRequest::delete()
            .uri(&format!("/api/v1/notifications/{id}"))
            .insert_header(AUTH)
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let request = test::TestRequest::get()
        .uri(&format!("/api/v1/notifications/{id}"))
        .insert_header(AUTH)
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let problem: Value = test::read_body_json(response).await;
    assert_eq!(problem["code"], "notification.gone");
}

/// Read whatever an open stream has produced so far, then stop waiting.
///
/// A stream response never ends on its own — that is the point of it — so a test
/// that called `read_body` would hang until the keep-alive outlived the runtime.
async fn read_available(response: actix_web::dev::ServiceResponse) -> String {
    use actix_web::body::MessageBody as _;

    let body = response.into_body();
    let mut body = std::pin::pin!(body);
    let mut collected = String::new();

    loop {
        let next = futures::future::poll_fn(|cx| body.as_mut().poll_next(cx));
        match tokio::time::timeout(std::time::Duration::from_millis(200), next).await {
            Ok(Some(Ok(chunk))) => collected.push_str(&String::from_utf8_lossy(&chunk)),
            // Timed out, ended, or errored: whatever the stream had, it has given.
            _ => return collected,
        }
    }
}

#[actix_web::test]
async fn the_stream_replays_the_backlog_with_the_cursor_as_the_event_id() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .set_json(json!({ "channel": "ci-infra", "title": "etcd degraded" }))
        .to_request();
    let published: Value = test::call_and_read_body_json(&app, request).await;
    let cursor = published["cursor"].as_str().unwrap();

    // Resume from before everything: the connection must catch up first and go live
    // second, or a client that reconnects loses whatever landed in between.
    let request = test::TestRequest::get()
        .uri("/api/v1/stream")
        .insert_header(AUTH)
        .insert_header(("Last-Event-ID", notify_domain::Cursor::START.to_string()))
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );

    let body = read_available(response).await;

    assert!(body.contains("event: revision"), "got: {body}");
    assert!(
        body.contains(&format!("id: {cursor}")),
        "the SSE id must be the cursor Last-Event-ID replays; got: {body}"
    );
    assert!(body.contains("etcd degraded"), "got: {body}");
}

#[actix_web::test]
async fn a_stream_resumed_from_the_head_does_not_replay_what_the_client_has() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .set_json(json!({ "channel": "ci-infra", "title": "already seen" }))
        .to_request();
    let published: Value = test::call_and_read_body_json(&app, request).await;

    let request = test::TestRequest::get()
        .uri("/api/v1/stream")
        .insert_header(AUTH)
        .insert_header(("Last-Event-ID", published["cursor"].as_str().unwrap()))
        .to_request();
    let body = read_available(test::call_service(&app, request).await).await;

    assert!(
        !body.contains("already seen"),
        "resuming at a cursor means after it, not from it: {body}"
    );
}

#[actix_web::test]
async fn a_stream_frame_says_which_channel_it_belongs_to() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .set_json(json!({ "channel": "ci-infra", "title": "etcd degraded" }))
        .to_request();
    let _: Value = test::call_and_read_body_json(&app, request).await;

    let request = test::TestRequest::get()
        .uri("/api/v1/stream")
        .insert_header(AUTH)
        .insert_header(("Last-Event-ID", notify_domain::Cursor::START.to_string()))
        .to_request();
    let body = read_available(test::call_service(&app, request).await).await;

    // The data line is the whole event (design §8.1), so a client that has never
    // seen this notification can file it without a follow-up request.
    let data = body
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("the stream produced no data line");
    let frame: Value = serde_json::from_str(data).unwrap();

    assert_eq!(frame["event"], "revision");
    assert_eq!(frame["audience"], "channel");
    assert_eq!(frame["channel_id"], "ci-infra");
    assert!(frame["cursor"].is_string(), "{frame}");
    assert!(frame["payload"]["title"] == "etcd degraded", "{frame}");
}

#[actix_web::test]
async fn a_stream_frame_says_whether_the_reader_was_meant_to_be_left_alone() {
    // D22: the server holds the preference and applies the rule, so it answers the
    // question once — per reader, on the event. Every client that decided this for
    // itself was another copy of a rule to keep in step, and there were three.
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    // Quiet from now until well past this test.
    let request = test::TestRequest::put()
        .uri("/api/v1/preferences/dnd")
        .insert_header(AUTH)
        .set_json(json!({ "until": "2099-01-01T00:00:00Z" }))
        .to_request();
    assert!(
        test::call_service(&app, request)
            .await
            .status()
            .is_success()
    );

    let request = test::TestRequest::post()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .set_json(json!({ "channel": "ci-infra", "title": "etcd degraded" }))
        .to_request();
    let _: Value = test::call_and_read_body_json(&app, request).await;

    let request = test::TestRequest::get()
        .uri("/api/v1/stream")
        .insert_header(AUTH)
        .insert_header(("Last-Event-ID", notify_domain::Cursor::START.to_string()))
        .to_request();
    let body = read_available(test::call_service(&app, request).await).await;

    let frame: Value = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|frame| frame["event"] == "revision")
        .expect("the stream produced no revision frame");

    assert_eq!(
        frame["muted_by_dnd"], true,
        "the reader is in a quiet period, and the server is what knows it: {frame}"
    );
    // The event still arrives, whole. §7.4 suppresses the *popup*, never the
    // stream: an open client is a deliberate choice to look.
    assert_eq!(frame["payload"]["title"], "etcd degraded");
}

#[actix_web::test]
async fn a_stream_frame_outside_a_quiet_period_carries_no_flag_at_all() {
    // Absent rather than false: the field is skipped when it is not true, so a
    // client written before it existed parses the frame unchanged.
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .set_json(json!({ "channel": "ci-infra", "title": "etcd degraded" }))
        .to_request();
    let _: Value = test::call_and_read_body_json(&app, request).await;

    let request = test::TestRequest::get()
        .uri("/api/v1/stream")
        .insert_header(AUTH)
        .insert_header(("Last-Event-ID", notify_domain::Cursor::START.to_string()))
        .to_request();
    let body = read_available(test::call_service(&app, request).await).await;

    let frame: Value = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|frame| frame["event"] == "revision")
        .expect("the stream produced no revision frame");

    assert!(frame["muted_by_dnd"].is_null(), "{frame}");
}

#[actix_web::test]
async fn a_reader_cannot_hold_more_connections_than_section_8_2_allows() {
    // The ceiling reaching an actual client. §8.2 publishes "connexions SSE/WS par
    // user: 20" and nothing enforced it: both transports served every connection
    // asked for. The registry is unit-tested next door; what this adds is that the
    // handler consults it, and that a refusal is the `429` with the `Retry-After`
    // §8.2 names rather than a stream that opens and stays silent.
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let open = async |app: &_| {
        let request = test::TestRequest::get()
            .uri("/api/v1/stream")
            .insert_header(AUTH)
            .to_request();
        test::call_service(app, request).await
    };

    // Held, not dropped: each response owns the receiving half of its stream, so
    // letting one go would hand its slot straight back and the twenty-first would
    // fit. Holding them is what makes this a test of the ceiling.
    let mut held = Vec::new();
    for n in 0..notify_domain::limits::MAX_STREAMS_PER_PRINCIPAL {
        let response = open(&app).await;
        assert_eq!(response.status(), StatusCode::OK, "connection {n} of 20");
        held.push(response);
    }

    let refused = open(&app).await;

    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        refused
            .headers()
            .get("retry-after")
            .expect("a 429 without Retry-After turns a polite client into a busy loop")
            .to_str()
            .unwrap(),
        "30"
    );

    let problem: Value = test::read_body_json(refused).await;
    assert_eq!(problem["code"], "rate.limited");
}

#[actix_web::test]
async fn the_websocket_shares_the_connection_ceiling_with_the_stream() {
    // One ceiling for both, as §8.2 writes it ("connexions SSE/WS"). Two counters
    // would mean forty, and a client that alternates transports would never be
    // refused by either.
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let mut held = Vec::new();
    for _ in 0..notify_domain::limits::MAX_STREAMS_PER_PRINCIPAL {
        let request = test::TestRequest::get()
            .uri("/api/v1/stream")
            .insert_header(AUTH)
            .to_request();
        held.push(test::call_service(&app, request).await);
    }

    let request = test::TestRequest::get()
        .uri("/api/v1/ws")
        .insert_header(AUTH)
        .to_request();
    let refused = test::call_service(&app, request).await;

    assert_eq!(
        refused.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "SSE connections fill the socket ceiling too"
    );
}

#[actix_web::test]
async fn a_stream_needs_a_credential_like_everything_else() {
    let (_, state) = Harness::new();
    let app = service!(state);

    let request = test::TestRequest::get().uri("/api/v1/stream").to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[actix_web::test]
async fn whoami_answers_with_the_callers_own_id() {
    // What a client needs to name itself as the `owner` of something it is about
    // to create — a personal API key, in particular — without the browser ever
    // learning it any other way (the OIDC `sub` claim is a different identifier).
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::get()
        .uri("/api/v1/me")
        .insert_header(AUTH)
        .to_request();
    let answer: Value = test::call_and_read_body_json(&app, request).await;

    assert!(
        answer["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("usr_")),
        "{answer}"
    );
}

#[actix_web::test]
async fn a_profile_edit_claims_the_field_and_a_null_hands_it_back() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    // What the identity provider said, before anybody touched it.
    let request = test::TestRequest::get()
        .uri("/api/v1/users/me/profile")
        .insert_header(AUTH)
        .to_request();
    let profile: Value = test::call_and_read_body_json(&app, request).await;
    assert_eq!(profile["display_name"], "Alice Martin");
    assert!(
        profile["overridden"].as_array().is_none_or(Vec::is_empty),
        "a profile nobody has edited claims nothing: {profile}"
    );

    // Claiming two fields.
    let request = test::TestRequest::put()
        .uri("/api/v1/users/me/profile")
        .insert_header(AUTH)
        .set_json(json!({ "display_name": "alice", "timezone": "Europe/Paris" }))
        .to_request();
    let profile: Value = test::call_and_read_body_json(&app, request).await;

    assert_eq!(profile["display_name"], "alice");
    assert_eq!(profile["timezone"], "Europe/Paris");
    let claimed: Vec<&str> = profile["overridden"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(claimed.contains(&"display_name") && claimed.contains(&"timezone"));

    // An absent field is left alone: this edit only mentions the locale.
    let request = test::TestRequest::put()
        .uri("/api/v1/users/me/profile")
        .insert_header(AUTH)
        .set_json(json!({ "locale": "fr-FR" }))
        .to_request();
    let profile: Value = test::call_and_read_body_json(&app, request).await;
    assert_eq!(profile["display_name"], "alice");
    assert_eq!(profile["locale"], "fr-FR");

    // An explicit null hands the field back to the provider rather than blanking
    // it — the way out of a name somebody typed once (§2.4).
    let request = test::TestRequest::put()
        .uri("/api/v1/users/me/profile")
        .insert_header(AUTH)
        .set_json(json!({ "display_name": null }))
        .to_request();
    let profile: Value = test::call_and_read_body_json(&app, request).await;

    let claimed: Vec<&str> = profile["overridden"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(!claimed.contains(&"display_name"), "{profile}");
}

#[actix_web::test]
async fn an_avatar_is_decoded_and_resized_before_it_is_ever_stored() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    // A 900×300 JPEG: neither square nor PNG, so what comes back proves the bytes
    // were processed rather than passed through (§2.4).
    let source = {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(900, 300));
        let mut out = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .expect("the fixture encodes");
        out.into_inner()
    };

    let request = test::TestRequest::put()
        .uri("/api/v1/users/me/avatar")
        .insert_header(AUTH)
        .insert_header(("content-type", "application/octet-stream"))
        .set_payload(source)
        .to_request();
    let profile: Value = test::call_and_read_body_json(&app, request).await;

    let id = profile["avatar"].as_str().expect("the profile names it");

    // And it is servable by that id.
    let request = test::TestRequest::get()
        .uri(&format!("/api/v1/avatars/{id}"))
        .insert_header(AUTH)
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "image/png",
        "whatever went in, a PNG comes out — which is what strips the metadata"
    );

    let bytes = test::read_body(response).await;
    let stored = image::load_from_memory(&bytes).expect("a decodable image");
    assert_eq!((stored.width(), stored.height()), (256, 256));
}

#[actix_web::test]
async fn something_that_is_not_an_image_never_reaches_the_bucket() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::put()
        .uri("/api/v1/users/me/avatar")
        .insert_header(AUTH)
        .set_payload(b"#!/bin/sh\necho not an image\n".to_vec())
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem: Value = test::read_body_json(response).await;
    assert_eq!(problem["code"], "field.malformed");
}

#[actix_web::test]
async fn a_timezone_the_database_does_not_know_is_refused() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::put()
        .uri("/api/v1/users/me/profile")
        .insert_header(AUTH)
        .set_json(json!({ "timezone": "Europe/Pris" }))
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem: Value = test::read_body_json(response).await;
    assert_eq!(problem["code"], "field.malformed");
}

#[actix_web::test]
async fn a_service_account_is_created_listed_and_deactivated() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::post()
        .uri("/api/v1/serviceaccounts")
        .insert_header(AUTH)
        .set_json(json!({ "id": "bot-argocd", "display_name": "ArgoCD" }))
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let created: Value = test::read_body_json(response).await;
    assert_eq!(created["id"], "bot-argocd");
    assert_eq!(created["origin"], "dynamic");
    assert!(
        created["created_by"].is_string(),
        "a bot nobody owns is a bot nobody can turn off: {created}"
    );

    let request = test::TestRequest::get()
        .uri("/api/v1/serviceaccounts")
        .insert_header(AUTH)
        .to_request();
    let listed: Value = test::call_and_read_body_json(&app, request).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);

    // The same name twice is refused rather than adopted.
    let request = test::TestRequest::post()
        .uri("/api/v1/serviceaccounts")
        .insert_header(AUTH)
        .set_json(json!({ "id": "bot-argocd" }))
        .to_request();
    assert_eq!(
        test::call_service(&app, request).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let request = test::TestRequest::delete()
        .uri("/api/v1/serviceaccounts/bot-argocd")
        .insert_header(AUTH)
        .to_request();
    assert_eq!(
        test::call_service(&app, request).await.status(),
        StatusCode::NO_CONTENT
    );

    // Deactivated, not deleted: it stays listed so the revisions naming it still
    // resolve, with the date it stopped being able to act.
    let request = test::TestRequest::get()
        .uri("/api/v1/serviceaccounts")
        .insert_header(AUTH)
        .to_request();
    let listed: Value = test::call_and_read_body_json(&app, request).await;
    assert!(listed[0]["disabled_at"].is_string(), "{listed}");
}

#[actix_web::test]
async fn patching_a_policy_touches_only_what_the_body_names() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::patch()
        .uri("/api/v1/channels/ci-infra")
        .insert_header(AUTH)
        .set_json(json!({ "max_ttl": "48h" }))
        .to_request();
    let channel: Value = test::call_and_read_body_json(&app, request).await;

    // `2d` and not `48h`: a duration is normalised to its largest whole unit on the
    // way back out, which is the same value said the shortest way.
    assert_eq!(channel["policy"]["ttl"]["max"], "2d");
    assert_eq!(
        channel["policy"]["ttl"]["default"], "1w",
        "a PATCH that names a ceiling must not reset the default"
    );
    assert_eq!(channel["policy"]["receipts_enabled"], false);

    // `"0"` lifts it. Sending `null` would be the shape no generated client can
    // express, which is why the API does not ask for one.
    let request = test::TestRequest::patch()
        .uri("/api/v1/channels/ci-infra")
        .insert_header(AUTH)
        .set_json(json!({ "max_ttl": "0" }))
        .to_request();
    let channel: Value = test::call_and_read_body_json(&app, request).await;

    assert!(
        channel["policy"]["ttl"]["max"].is_null(),
        "a ceiling that cannot be lifted is a channel stuck with its first admin's \
         guess: {channel}"
    );
}

#[actix_web::test]
async fn critical_t15_a_payload_over_the_ceiling_is_refused_with_413() {
    // T15's first half at the surface a publisher meets. The domain refuses it; what
    // this pins is the **status code** §8.2 promises — a publisher retrying a 500
    // forever because a 413 was mapped wrong is the failure worth a test.
    let (harness, state) = Harness::new();
    harness.with_alice_in("cluster-ovh-1").await;

    let app = service!(state);
    let request = test::TestRequest::post()
        .uri("/api/v1/notifications")
        .insert_header(AUTH)
        .set_json(json!({
            "channel": "cluster-ovh-1",
            "title": "far too much",
            // Multi-byte on purpose: the body limit counts characters, this one
            // counts bytes, and the gap between the two is where an oversized
            // payload gets through.
            "body": "é".repeat(16 * 1024),
            "image": {
                "kind": "data_uri",
                "uri": format!("data:image/png;base64,{}", "A".repeat(47 * 1024)),
            },
        }))
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let problem: Value = test::read_body_json(response).await;
    assert_eq!(
        problem["code"], "payload.too_large",
        "the stable code clients switch on (§8), not just the status"
    );
}

#[actix_web::test]
async fn critical_t15_a_presign_over_the_ceiling_is_refused_before_it_is_signed() {
    // T15's second half. Refused **before** signing: a signature handed out for an
    // upload the bucket will reject is a round trip and a confusing error, and the
    // ceiling is ours to enforce rather than the store's to discover.
    let (harness, state) = Harness::new();
    harness.with_alice_in("cluster-ovh-1").await;

    let app = service!(state);
    let request = test::TestRequest::post()
        .uri("/api/v1/attachments/presign")
        .insert_header(AUTH)
        .set_json(json!({
            "channel": "cluster-ovh-1",
            "content_type": "image/png",
            "content_length": 26 * 1024 * 1024_u64,
        }))
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[actix_web::test]
async fn a_channel_can_be_read_by_id_rather_than_hunted_for_in_a_page() {
    // §8 has listed this route since M1 and no build served it: a client wanting
    // one channel had to walk `GET /channels` until it found it.
    let (harness, state) = Harness::new();
    harness.with_alice_in("cluster-ovh-1").await;

    let app = service!(state);
    let request = test::TestRequest::get()
        .uri("/api/v1/channels/cluster-ovh-1")
        .insert_header(AUTH)
        .to_request();
    let channel: serde_json::Value = test::call_and_read_body_json(&app, request).await;

    assert_eq!(channel["id"], "cluster-ovh-1");
    assert!(
        channel["members"].as_array().is_some_and(|m| !m.is_empty()),
        "the detail §8 promises is members and policy, not just the id: {channel}"
    );
    assert!(channel["policy"].is_object());
    assert_eq!(
        channel["origin"], "dynamic",
        "the managed badge §8 asks for is this field"
    );
}

#[actix_web::test]
async fn reading_a_channel_you_are_not_in_says_nothing_about_whether_it_exists() {
    // The rule `Authorizer` holds everywhere else, checked on the new door: the
    // answer for "not yours" and for "not there" has to be the same one, or this
    // route becomes the cheapest way to enumerate an instance.
    let (harness, state) = Harness::new();
    harness.with_alice_in("cluster-ovh-1").await;

    let secret = Channel::dynamic(ChannelId::parse("someone-elses").unwrap(), None).unwrap();
    ChannelRepository::upsert(&harness.store, &secret)
        .await
        .unwrap();

    let app = service!(state);
    let existing = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/channels/someone-elses")
            .insert_header(AUTH)
            .to_request(),
    )
    .await;
    let absent = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/channels/never-existed")
            .insert_header(AUTH)
            .to_request(),
    )
    .await;

    assert_eq!(existing.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        absent.status(),
        existing.status(),
        "one of these two answers would tell an attacker which channels exist"
    );
}

#[actix_web::test]
async fn patching_a_managed_channel_is_refused_like_deleting_one() {
    let (harness, state) = Harness::new();
    let id = harness.with_alice_in("cluster-ovh-1").await;

    let mut managed = ChannelRepository::get(&harness.store, &id)
        .await
        .unwrap()
        .unwrap();
    managed.origin = Origin::Managed;
    ChannelRepository::upsert(&harness.store, &managed)
        .await
        .unwrap();

    let app = service!(state);
    let request = test::TestRequest::patch()
        .uri("/api/v1/channels/cluster-ovh-1")
        .insert_header(AUTH)
        .set_json(json!({ "receipts_enabled": true }))
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "config owns a managed channel's policy (§11)"
    );
}

#[actix_web::test]
async fn a_policy_the_channel_could_not_publish_under_is_refused() {
    // `forever` on a channel that does not allow it would fail every publish from
    // now on, a long way from the request that caused it.
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::patch()
        .uri("/api/v1/channels/ci-infra")
        .insert_header(AUTH)
        .set_json(json!({ "default_ttl": "0" }))
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[actix_web::test]
async fn the_openapi_document_covers_every_route_that_is_registered() {
    let (_, api) = App::new()
        .into_utoipa_app()
        .configure(notify_http_actix::configure)
        .split_for_parts();

    let paths: Vec<&String> = api.paths.paths.keys().collect();

    for expected in [
        "/api/v1/notifications",
        "/api/v1/notifications/{id}",
        "/api/v1/notifications/{id}/revisions",
        "/api/v1/notifications/{id}/interactions",
        "/api/v1/notifications/{id}/silence",
        "/api/v1/channels",
        "/api/v1/channels/{channel}",
        "/api/v1/channels/{channel}/members",
        "/api/v1/channels/{channel}/members/{principal}",
        "/api/v1/channels/{channel}/notifications",
        "/api/v1/channels/{channel}/notifications/{external_id}",
        "/api/v1/search",
        "/api/v1/sync",
        "/api/v1/stream",
        "/api/v1/ws",
        "/api/v1/receipts",
        "/api/v1/notifications/{id}/receipts",
        "/api/v1/auth/token/exchange",
        "/api/v1/admin/audit",
        "/api/v1/admin/channels",
        "/api/v1/admin/channels/{channel}/join",
        "/api/v1/admin/trust-policies",
        "/api/v1/admin/reconciliation",
        "/api/v1/attachments/presign",
        "/api/v1/attachments/{id}",
        "/api/v1/attachments/{id}/thumbnail",
        "/api/v1/devices",
        "/api/v1/devices/{id}",
        "/api/v1/apikeys",
        "/api/v1/apikeys/{id}",
        "/api/v1/apikeys/{id}/limit",
        "/api/v1/users/me/profile",
        "/api/v1/users/me/avatar",
        "/api/v1/avatars/{id}",
        "/api/v1/serviceaccounts",
        "/api/v1/serviceaccounts/{id}",
    ] {
        assert!(
            paths.iter().any(|path| path.as_str() == expected),
            "{expected} is served but undocumented; the generated clients would not have it"
        );
    }
}

#[actix_web::test]
async fn every_operation_design_8_lists_is_served_and_not_only_its_path() {
    // The test above checks *paths*, and that is how `GET /channels/{channel}` went
    // missing for four milestones: `PATCH` and `DELETE` registered the same path, so
    // the list was satisfied while the read §8 promises was served by nothing.
    //
    // This one is method-aware, and it is maintained by hand from §8 on purpose —
    // that list is prose, so the only way it can fail a build is if somebody copies
    // it here. Copying it here is the cheap half; noticing was the expensive half.
    let (_, api) = App::new()
        .into_utoipa_app()
        .configure(notify_http_actix::configure)
        .split_for_parts();

    for (method, path) in [
        ("get", "/api/v1/channels"),
        ("post", "/api/v1/channels"),
        ("get", "/api/v1/channels/{channel}"),
        ("patch", "/api/v1/channels/{channel}"),
        ("delete", "/api/v1/channels/{channel}"),
        ("post", "/api/v1/channels/{channel}/members"),
        ("delete", "/api/v1/channels/{channel}/members/{principal}"),
        ("get", "/api/v1/channels/{channel}/notifications"),
        (
            "put",
            "/api/v1/channels/{channel}/notifications/{external_id}",
        ),
        ("post", "/api/v1/notifications"),
        ("get", "/api/v1/notifications/{id}"),
        ("delete", "/api/v1/notifications/{id}"),
        ("post", "/api/v1/notifications/{id}/interactions"),
        ("get", "/api/v1/notifications/{id}/interactions"),
        ("post", "/api/v1/notifications/{id}/silence"),
        ("get", "/api/v1/notifications/{id}/receipts"),
        ("get", "/api/v1/notifications/{id}/revisions"),
        ("get", "/api/v1/search"),
        ("get", "/api/v1/sync"),
        ("get", "/api/v1/stream"),
        ("get", "/api/v1/ws"),
        ("post", "/api/v1/receipts"),
        ("get", "/api/v1/push/webpush"),
        ("get", "/api/v1/preferences/dnd"),
        ("put", "/api/v1/preferences/dnd"),
        ("get", "/api/v1/devices"),
        ("post", "/api/v1/devices"),
        ("delete", "/api/v1/devices/{id}"),
        ("get", "/api/v1/users/me/profile"),
        ("put", "/api/v1/users/me/profile"),
        ("put", "/api/v1/users/me/avatar"),
        ("get", "/api/v1/serviceaccounts"),
        ("post", "/api/v1/serviceaccounts"),
        ("delete", "/api/v1/serviceaccounts/{id}"),
        ("get", "/api/v1/apikeys"),
        ("post", "/api/v1/apikeys"),
        ("delete", "/api/v1/apikeys/{id}"),
        ("patch", "/api/v1/apikeys/{id}/limit"),
        ("post", "/api/v1/auth/token/exchange"),
        ("post", "/api/v1/attachments/presign"),
        ("get", "/api/v1/attachments/{id}"),
        ("get", "/api/v1/attachments/{id}/thumbnail"),
        ("get", "/api/v1/admin/audit"),
        ("get", "/api/v1/admin/channels"),
        ("post", "/api/v1/admin/channels/{channel}/join"),
        ("get", "/api/v1/admin/trust-policies"),
        ("get", "/api/v1/admin/dlq"),
        ("post", "/api/v1/admin/dlq/{id}/requeue"),
        ("get", "/api/v1/admin/reconciliation"),
    ] {
        let item = api.paths.paths.get(path).unwrap_or_else(|| {
            unreachable!("§8 lists {method} {path} and no route serves that path")
        });

        let served: Vec<&str> = [
            ("get", item.get.is_some()),
            ("put", item.put.is_some()),
            ("post", item.post.is_some()),
            ("delete", item.delete.is_some()),
            ("patch", item.patch.is_some()),
        ]
        .into_iter()
        .filter_map(|(verb, present)| present.then_some(verb))
        .collect();

        assert!(
            served.contains(&method),
            "§8 lists {method} {path}; that path serves only {served:?}"
        );
    }
}

// --- Receipts (design §7.3, §10.3) ------------------------------------------

/// Publish one notification into a channel and hand back its id.
macro_rules! publish_into {
    ($app:expr, $channel:literal) => {{
        let request = test::TestRequest::post()
            .uri("/api/v1/notifications")
            .insert_header(AUTH)
            .set_json(json!({ "channel": $channel, "title": "etcd degraded" }))
            .to_request();
        let published: Value = test::call_and_read_body_json(&$app, request).await;
        published["notification"]["id"].as_str().unwrap().to_owned()
    }};
}

#[actix_web::test]
async fn the_roll_names_the_people_who_read_it() {
    let (harness, state) = Harness::new();
    let id = harness.with_alice_in("ci-infra").await;

    let mut channel = ChannelRepository::get(&harness.store, &id)
        .await
        .unwrap()
        .unwrap();
    channel.policy.receipts_enabled = true;
    ChannelRepository::upsert(&harness.store, &channel)
        .await
        .unwrap();

    let app = service!(state);
    let notification = publish_into!(app, "ci-infra");

    let request = test::TestRequest::post()
        .uri("/api/v1/receipts")
        .insert_header(AUTH)
        .set_json(json!({ "notification_id": notification, "kind": "seen" }))
        .to_request();
    assert!(
        test::call_service(&app, request)
            .await
            .status()
            .is_success()
    );

    let request = test::TestRequest::get()
        .uri(&format!("/api/v1/notifications/{notification}/receipts"))
        .insert_header(AUTH)
        .to_request();
    let roll: Value = test::call_and_read_body_json(&app, request).await;

    assert_eq!(roll["audience"], 1);
    assert_eq!(roll["seen"].as_array().unwrap().len(), 1);
    assert_eq!(
        roll["seen"][0]["author"]["display_name"], "Alice Martin",
        "a hover card of raw identifiers answers nothing (§2.4): {roll}"
    );
    assert_eq!(roll["seen"][0]["author"]["kind"], "user");
    assert_eq!(
        roll["delivered_devices"], 0,
        "delivery is folded, and nothing delivered here"
    );
}

#[actix_web::test]
async fn a_channel_that_collects_no_receipts_answers_null_rather_than_zero() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);
    let notification = publish_into!(app, "ci-infra");

    let request = test::TestRequest::get()
        .uri(&format!("/api/v1/notifications/{notification}/receipts"))
        .insert_header(AUTH)
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = test::read_body_json(response).await;
    assert!(
        body.is_null(),
        "\"nobody read it\" and \"we do not record that\" must not render the same: {body}"
    );
}

// --- Do not disturb (design §7.4) -------------------------------------------

#[actix_web::test]
async fn a_do_not_disturb_reads_back_as_it_was_set() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    // Never set: a preference that mutes nothing, not a 404.
    let request = test::TestRequest::get()
        .uri("/api/v1/preferences/dnd")
        .insert_header(AUTH)
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let dnd: Value = test::read_body_json(response).await;
    assert!(dnd["quiet_hours"].as_array().is_none_or(Vec::is_empty));

    let request = test::TestRequest::put()
        .uri("/api/v1/preferences/dnd")
        .insert_header(AUTH)
        .set_json(json!({
            "quiet_hours": [{
                "days": ["mon", "fri"],
                "start": "22:00",
                "end": "08:00",
                "timezone": "Europe/Paris"
            }],
            "exceptions": ["cluster-ovh-1"],
            "urgent_bypasses": true
        }))
        .to_request();
    let stored: Value = test::call_and_read_body_json(&app, request).await;

    assert_eq!(stored["quiet_hours"][0]["start"], "22:00");
    assert_eq!(stored["exceptions"][0], "cluster-ovh-1");
    assert_eq!(stored["urgent_bypasses"], true);

    let request = test::TestRequest::get()
        .uri("/api/v1/preferences/dnd")
        .insert_header(AUTH)
        .to_request();
    let read_back: Value = test::call_and_read_body_json(&app, request).await;
    assert_eq!(read_back, stored, "what was stored is what comes back");
}

#[actix_web::test]
async fn a_do_not_disturb_is_set_on_the_caller_who_never_had_to_name_themselves() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    // An empty body is a complete one: the subject is not something a caller says,
    // and requiring it would mean a client that does not know its own id could not
    // build a request at all.
    let request = test::TestRequest::put()
        .uri("/api/v1/preferences/dnd")
        .insert_header(AUTH)
        .set_json(json!({}))
        .to_request();
    let stored: Value = test::call_and_read_body_json(&app, request).await;

    assert_eq!(stored["user"], harness.alice.to_string());
}

#[actix_web::test]
async fn a_window_the_timezone_database_does_not_know_is_refused() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::put()
        .uri("/api/v1/preferences/dnd")
        .insert_header(AUTH)
        .set_json(json!({
            "quiet_hours": [{
                "start": "22:00",
                "end": "08:00",
                "timezone": "Europe/Pris"
            }]
        }))
        .to_request();
    let response = test::call_service(&app, request).await;

    // Refused at the door: accepted here, it becomes a notification that never
    // arrives in six months with nothing to connect the two.
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[actix_web::test]
async fn a_do_not_disturb_needs_a_credential() {
    let (_harness, state) = Harness::new();
    let app = service!(state);

    let request = test::TestRequest::get()
        .uri("/api/v1/preferences/dnd")
        .to_request();

    assert_eq!(
        test::call_service(&app, request).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[actix_web::test]
async fn a_browser_is_told_the_key_it_has_to_subscribe_with() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::get()
        .uri("/api/v1/push/webpush")
        .insert_header(AUTH)
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, request).await;

    assert_eq!(
        body["application_server_key"], APPLICATION_SERVER_KEY,
        "verbatim: it is passed straight to `pushManager.subscribe()`"
    );
}

#[actix_web::test]
async fn a_deployment_without_web_push_says_so_instead_of_inventing_a_key() {
    let (harness, mut state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    state.application_server_key = None;
    let app = service!(state);

    let request = test::TestRequest::get()
        .uri("/api/v1/push/webpush")
        .insert_header(AUTH)
        .to_request();
    let response = test::call_service(&app, request).await;

    // The same answer every unconfigured capability gives (§14.3), so a PWA can
    // skip the subscription rather than retry a key that will never come.
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
}

#[actix_web::test]
async fn a_deployment_without_the_cli_section_says_so_instead_of_downloading_nothing() {
    let (harness, state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    let app = service!(state);

    let request = test::TestRequest::get()
        .uri("/api/v1/cli")
        .insert_header(AUTH)
        .to_request();
    let response = test::call_service(&app, request).await;

    // The same answer every unconfigured capability gives (§14.3) — no `[cli]`
    // section means this deployment does not serve the binary.
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
}

#[actix_web::test]
async fn an_authenticated_caller_downloads_the_configured_cli_binary() {
    let (harness, mut state) = Harness::new();
    harness.with_alice_in("ci-infra").await;
    state.cli_binary = Some(web::Bytes::from_static(b"not really a binary, but bytes"));
    let app = service!(state);

    let request = test::TestRequest::get()
        .uri("/api/v1/cli")
        .insert_header(AUTH)
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-disposition")
            .expect("a download should not be left for the browser to name"),
        "attachment; filename=\"notify\""
    );
    let body = test::read_body(response).await;
    assert_eq!(body, "not really a binary, but bytes");
}
