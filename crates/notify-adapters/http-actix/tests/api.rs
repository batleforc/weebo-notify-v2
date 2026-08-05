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
    Authenticate, Authorizer, CreateApiKey, CreateChannel, CreateServiceAccount,
    DeactivateServiceAccount, DeleteChannel, DeleteNotification, EnrolDevice, Fanout,
    GetNotification, ListApiKeys, ListChannels, ListDevices, ListInbox, ListInteractions,
    ListRevisions, ListServiceAccounts, ManageMembers, PostReceipt, PublishNotification,
    ReadAvatar, ReadDnd, ReadProfile, RecordInteraction, ResolveStreamStart, RevokeApiKey,
    RevokeDevice, SearchNotifications, SetAvatar, SetDnd, SilenceNotification, SyncBacklog,
    UpdateChannel, UpdateProfile, UpsertNotification,
};
use notify_avatars::ImageAvatarProcessor;
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
        );
        let get = GetNotification::new(
            Arc::new(store.clone()),
            authorizer.clone(),
            Arc::new(clock.clone()),
        );

        let state = ApiState {
            bus: Arc::new(store.clone()),
            authenticate: Authenticate::new(
                Arc::new(tokens.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(StubSecretHasher),
                Arc::new(clock.clone()),
            ),
            publish: PublishNotification::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(limiter.clone()),
                Arc::new(clock.clone()),
                authorizer.clone(),
                fanout.clone(),
            ),
            upsert: UpsertNotification::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(limiter.clone()),
                Arc::new(clock.clone()),
                authorizer.clone(),
                fanout.clone(),
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
            record_interaction: RecordInteraction::new(
                Arc::new(store.clone()),
                get.clone(),
                Arc::new(clock.clone()),
                fanout.clone(),
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
                get,
                Arc::new(clock.clone()),
                fanout.clone(),
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
async fn a_stream_needs_a_credential_like_everything_else() {
    let (_, state) = Harness::new();
    let app = service!(state);

    let request = test::TestRequest::get().uri("/api/v1/stream").to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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
        "/api/v1/devices",
        "/api/v1/devices/{id}",
        "/api/v1/apikeys",
        "/api/v1/apikeys/{id}",
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
