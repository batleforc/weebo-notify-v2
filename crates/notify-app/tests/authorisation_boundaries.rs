//! Authorisation, written as attacks (SEC-P2-38): each test names an escalation and
//! asserts it is refused, so a refactor that widens a grant fails here.

#![allow(
    clippy::expect_used,
    reason = "a failing expect in a test is a failing test, and the message is the diagnosis"
)]

use std::sync::Arc;

use notify_app::{
    Authorizer, Caller, DeactivateServiceAccount, EnrolDevice, Fanout, GetNotification, Grant,
    ListApiKeys, ListDevices, PostReceipt, ReadAvatar, RevokeApiKey, RevokeDevice,
    SilenceNotification,
};
use notify_domain::{
    ApiKey, ApiKeyId, ApiKeyOwner, ApiKeyPrefix, AttachmentId, Author, Channel, ChannelId,
    ChannelPolicy, ContentDraft, Device, Membership, Notification, NotificationId, Origin,
    Principal, PushTransport, ReceiptKind, RevisionDraft, Scope, ServiceAccount, ServiceAccountId,
    SilenceScope, State, User, UserId,
};
use notify_ports::test_utils::{InMemory, InMemoryAttachments, TestClock};
use notify_ports::{
    AttachmentStore as _, Clock as _, DeviceRepository, NotificationRepository,
    ServiceAccountRepository, StoredApiKey,
};

fn channel_id(id: &str) -> ChannelId {
    ChannelId::parse(id).expect("a valid channel id")
}

fn user() -> Principal {
    Principal::User {
        id: UserId::generate(),
    }
}

fn channel_with(id: &str, member: &Principal, scopes: Vec<Scope>) -> Channel {
    let mut channel = Channel::dynamic(channel_id(id), None).expect("a dynamic channel");
    channel.upsert_member(Membership::new(member.clone(), scopes).expect("a membership"));
    channel
}

/// Two channels, so "may I reach the other one" is always askable.
fn two_channels(member: &Principal, scopes: Vec<Scope>) -> Authorizer {
    let store = InMemory::new();
    store.seed_channel(channel_with("ci-infra", member, scopes));
    store.seed_channel(
        Channel::dynamic(channel_id("payroll"), None).expect("the channel they are not in"),
    );
    Authorizer::new(Arc::new(store))
}

fn api_key(principal: Principal, scopes: Vec<Scope>, channels: Vec<&str>) -> Caller {
    Caller {
        principal,
        grant: Grant::ApiKey {
            id: ApiKeyId::generate(),
            scopes,
            channels: channels.into_iter().map(channel_id).collect(),
            publish_per_minute: None,
            owner_groups: Vec::new(),
        },
        groups: Vec::new(),
        platform_admin: false,
        expires_at: None,
    }
}

// ── Lateral movement between channels ────────────────────────────────────────

#[tokio::test]
async fn a_key_scoped_to_one_channel_cannot_reach_another() {
    // The escalation: a CI key that publishes build results also reads payroll.
    let owner = user();
    let authorizer = two_channels(&owner, vec![Scope::Publish, Scope::Subscribe]);
    let caller = api_key(
        owner,
        vec![Scope::Publish, Scope::Subscribe],
        vec!["ci-infra"],
    );

    for scope in [Scope::Subscribe, Scope::Publish, Scope::Manage] {
        assert!(
            authorizer
                .authorize(&caller, &channel_id("payroll"), scope)
                .await
                .is_err(),
            "a key naming only ci-infra was allowed {scope:?} on payroll"
        );
    }
}

#[tokio::test]
async fn a_members_own_rights_do_not_leak_into_their_key() {
    // A narrow key must not carry its owner's scopes (§5.2).
    let owner = user();
    let store = InMemory::new();
    store.seed_channel(channel_with(
        "ci-infra",
        &owner,
        vec![Scope::Publish, Scope::Subscribe, Scope::Manage],
    ));
    let authorizer = Authorizer::new(Arc::new(store));

    let narrow = api_key(owner.clone(), vec![Scope::Publish], vec!["ci-infra"]);

    assert!(
        authorizer
            .authorize(&narrow, &channel_id("ci-infra"), Scope::Publish)
            .await
            .is_ok()
    );
    for escalated in [Scope::Manage, Scope::Subscribe] {
        assert!(
            authorizer
                .authorize(&narrow, &channel_id("ci-infra"), escalated)
                .await
                .is_err(),
            "a publish-only key inherited {escalated:?} from its owner's membership"
        );
    }
}

#[tokio::test]
async fn a_key_naming_a_channel_it_was_never_granted_is_not_enough() {
    // Naming a channel the key carries still requires the scope.
    let owner = user();
    let authorizer = two_channels(&owner, vec![Scope::Subscribe]);
    let caller = api_key(owner, vec![Scope::Subscribe], vec!["ci-infra", "payroll"]);

    // A key's rights come from itself, not membership.
    assert!(
        authorizer
            .authorize(&caller, &channel_id("payroll"), Scope::Publish)
            .await
            .is_err(),
        "a subscribe-only key published to a channel merely because it was listed"
    );
}

// ── Group claims ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn groups_on_a_token_do_not_apply_to_an_api_key() {
    // Group claims on a key-authenticated caller must not open the group door.
    let owner = user();
    let store = InMemory::new();
    let mut channel = Channel::dynamic(channel_id("payroll"), None).expect("a channel");
    channel.upsert_member(
        Membership::new(owner.clone(), vec![Scope::Subscribe]).expect("a membership"),
    );
    store.seed_channel(channel);
    let authorizer = Authorizer::new(Arc::new(store));

    let mut caller = api_key(owner, vec![Scope::Publish], vec!["ci-infra"]);
    caller.groups = vec!["finance".to_owned(), "platform-admins".to_owned()];

    assert!(
        authorizer
            .authorize(&caller, &channel_id("payroll"), Scope::Subscribe)
            .await
            .is_err(),
        "group claims widened an API key's reach"
    );
}

#[tokio::test]
async fn an_empty_group_list_matches_no_group_binding() {
    // T29: no groups means individual bindings only, never all groups.
    let alice = user();
    let authorizer = two_channels(&alice, vec![Scope::Subscribe]);

    let mut caller = Caller::member(alice);
    caller.groups = Vec::new();

    assert!(
        authorizer
            .authorize(&caller, &channel_id("payroll"), Scope::Subscribe)
            .await
            .is_err()
    );
}

// ── Identity confusion ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_service_account_does_not_inherit_a_users_membership() {
    // The escalation: two principals that print similarly are treated as one.
    let alice = user();
    let authorizer = two_channels(&alice, vec![Scope::Publish, Scope::Subscribe]);

    let bot = Caller::member(Principal::ServiceAccount {
        id: ServiceAccountId::parse("bot-argocd").expect("a valid service account id"),
    });

    assert!(
        authorizer
            .authorize(&bot, &channel_id("ci-infra"), Scope::Subscribe)
            .await
            .is_err(),
        "a service account was authorised by a user's membership"
    );
}

#[tokio::test]
async fn one_users_membership_does_not_authorise_another() {
    let alice = user();
    let authorizer = two_channels(&alice, vec![Scope::Publish, Scope::Subscribe]);
    let mallory = Caller::member(user());

    assert!(
        authorizer
            .authorize(&mallory, &channel_id("ci-infra"), Scope::Subscribe)
            .await
            .is_err()
    );
}

// ── The platform role ────────────────────────────────────────────────────────

#[tokio::test]
async fn the_platform_role_is_not_a_channel_grant() {
    // The platform role (§5.4) is not a skeleton key for channel content.
    let alice = user();
    let authorizer = two_channels(&alice, vec![Scope::Subscribe]);

    let mut admin = Caller::member(user());
    admin.platform_admin = true;

    assert!(
        authorizer
            .authorize(&admin, &channel_id("payroll"), Scope::Subscribe)
            .await
            .is_err(),
        "the platform role opened a channel its holder is not a member of"
    );
}

// ── The shape of a refusal ───────────────────────────────────────────────────

#[tokio::test]
async fn a_refusal_never_reveals_whether_the_channel_exists() {
    // "No such channel" and "not allowed" must be one answer.
    let alice = user();
    let authorizer = two_channels(&alice, vec![Scope::Subscribe]);
    let caller = Caller::member(alice);

    let real_but_forbidden = authorizer
        .authorize(&caller, &channel_id("payroll"), Scope::Subscribe)
        .await
        .expect_err("payroll exists and this caller is not in it");
    let does_not_exist = authorizer
        .authorize(&caller, &channel_id("no-such-channel"), Scope::Subscribe)
        .await
        .expect_err("this channel was never seeded");

    assert_eq!(
        real_but_forbidden.code(),
        does_not_exist.code(),
        "the refusal codes differ, so a caller can enumerate which channels exist"
    );
    assert_eq!(
        real_but_forbidden.to_string(),
        does_not_exist.to_string(),
        "the refusal messages differ, which is the same oracle one layer down"
    );
}

// ── What a key may do to its own owner ───────────────────────────────────────
//
// A personal key acts *as* its owner, so "is it the owner's" is always yes. The
// question these ask is whether one leaked key — typically a publish-only one
// sitting in a CI variable — can be turned against the owner's other credentials
// and their delivery.

fn alice_and_her_key() -> (UserId, Caller, Caller) {
    let alice = UserId::generate();
    let principal = Principal::User { id: alice };
    let key = api_key(principal.clone(), vec![Scope::Publish], vec!["ci-infra"]);
    (alice, Caller::member(principal), key)
}

fn webpush() -> PushTransport {
    PushTransport::WebPush {
        endpoint: url::Url::parse("https://push.example/abc").expect("a url"),
        p256dh: "BPk".to_owned(),
        auth: "k9".to_owned(),
    }
}

fn stored_key(owner: ApiKeyOwner, prefix: &str) -> StoredApiKey {
    StoredApiKey {
        key: ApiKey {
            id: ApiKeyId::generate(),
            prefix: ApiKeyPrefix::parse(prefix).expect("a prefix"),
            label: None,
            owner,
            scopes: vec![Scope::Publish],
            channels: vec![channel_id("ci-infra")],
            publish_per_minute: None,
            created_at: TestClock::new().now(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        },
        secret_hash: "hash".to_owned(),
    }
}

#[tokio::test]
async fn a_key_cannot_enrol_a_push_device_that_receives_its_owners_notifications() {
    // The escalation: a leaked publish-only key enrols the attacker's web-push
    // endpoint as the owner's device, and every notification the owner can read is
    // now delivered to the attacker.
    let (_, _, key) = alice_and_her_key();
    let store = InMemory::new();
    let clock = Arc::new(TestClock::new());
    let enrol = EnrolDevice::new(Arc::new(store.clone()), Arc::new(store.clone()), clock);

    assert!(
        enrol
            .execute(&key, "attacker", webpush(), None)
            .await
            .is_err(),
        "an API key enrolled a push device for its owner"
    );
    assert!(
        enrol
            .execute(&key, "cli", PushTransport::Stream, None)
            .await
            .is_ok(),
        "the CLI's own stream device is still a key's to enrol"
    );
}

#[tokio::test]
async fn a_key_can_neither_see_nor_revoke_its_owners_push_devices() {
    let (alice, session, key) = alice_and_her_key();
    let store = InMemory::new();
    let clock = Arc::new(TestClock::new());
    let enrol = EnrolDevice::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        clock.clone(),
    );
    let phone = enrol
        .execute(&session, "phone", webpush(), None)
        .await
        .expect("a session enrols a push device");
    assert_eq!(phone.owner, Principal::User { id: alice });

    let listed = ListDevices::new(Arc::new(store.clone()))
        .execute(&key)
        .await
        .expect("listing is allowed");
    assert!(
        listed.iter().all(|device| device.id != phone.id),
        "a key read its owner's push endpoint"
    );

    let revoke = RevokeDevice::new(Arc::new(store.clone()), Arc::new(store.clone()), clock);
    assert!(
        revoke.execute(&key, phone.id).await.is_err(),
        "a key switched off its owner's phone"
    );
    let stored: Option<Device> = DeviceRepository::get(&store, phone.id)
        .await
        .expect("a lookup");
    assert!(stored.expect("still there").is_active());
}

#[tokio::test]
async fn a_key_cannot_list_or_revoke_its_owners_other_keys() {
    // The escalation: one leaked key enumerates and switches off the rest.
    let (alice, session, key) = alice_and_her_key();
    let store = InMemory::new();
    let sibling = stored_key(ApiKeyOwner::User { id: alice }, "wnk1sibl");
    let sibling_id = sibling.key.id;
    store.seed_api_key(sibling);
    let authorizer = Authorizer::new(Arc::new(store.clone()));

    let list = ListApiKeys::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        authorizer.clone(),
    );
    assert!(
        list.execute(&key).await.is_err(),
        "a key listed its owner's keys"
    );
    assert_eq!(
        list.execute(&session).await.expect("a session lists").len(),
        1
    );

    let revoke = RevokeApiKey::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        authorizer,
        Arc::new(store.clone()),
        Arc::new(TestClock::new()),
    );
    assert!(
        revoke.execute(&key, sibling_id).await.is_err(),
        "a key revoked a sibling key"
    );

    // …but "this leaked, kill it" still works from the leaked key itself.
    let Grant::ApiKey { id: own, .. } = key.grant.clone() else {
        unreachable!("api_key builds an ApiKey grant")
    };
    let mut own_key = stored_key(ApiKeyOwner::User { id: alice }, "wnk1ownk");
    own_key.key.id = own;
    store.seed_api_key(own_key);
    assert!(revoke.execute(&key, own).await.expect("self-revocation"));
}

#[tokio::test]
async fn a_key_cannot_deactivate_the_service_account_its_owner_created() {
    // The escalation: the creator's leaked key turns a production pipeline off.
    let (alice, session, key) = alice_and_her_key();
    let store = InMemory::new();
    let clock = TestClock::new();
    let account = ServiceAccount::new(
        ServiceAccountId::parse("bot-argocd").expect("an id"),
        None,
        None,
        Origin::Dynamic,
        Some(alice),
        clock.now(),
    )
    .expect("an account");
    ServiceAccountRepository::upsert(&store, &account)
        .await
        .expect("stored");

    let deactivate = DeactivateServiceAccount::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(clock),
    );
    assert!(deactivate.execute(&key, &account.id).await.is_err());
    let after = ServiceAccountRepository::get(&store, &account.id)
        .await
        .expect("a lookup")
        .expect("still there");
    assert!(
        after.disabled_at.is_none(),
        "a key deactivated a service account"
    );

    assert!(deactivate.execute(&session, &account.id).await.is_ok());
}

/// A notification in `ci-infra` that Alice and her key can both read, and the
/// two use cases that act on it.
async fn alice_reading_one_notification(
    alice: &Principal,
) -> (InMemory, NotificationId, SilenceNotification, PostReceipt) {
    let store = InMemory::new();
    let clock = Arc::new(TestClock::new());
    store.seed_channel(channel_with(
        "ci-infra",
        alice,
        vec![Scope::Publish, Scope::Subscribe],
    ));
    let (notification, written) = Notification::create(
        NotificationId::generate(),
        channel_id("ci-infra"),
        None,
        RevisionDraft::lifecycle(State::Open, ContentDraft::titled("etcd degraded")),
        Author::anonymous(alice.clone()),
        clock.now(),
        &ChannelPolicy::default(),
    )
    .expect("a notification");
    NotificationRepository::insert(&store, &notification, &written.revision)
        .await
        .expect("stored");

    let get = GetNotification::new(
        Arc::new(store.clone()),
        Authorizer::new(Arc::new(store.clone())),
        clock.clone(),
    );
    let fanout = Fanout::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        clock.clone(),
        4_096,
    );
    let silence = SilenceNotification::new(
        Arc::new(store.clone()),
        get.clone(),
        clock.clone(),
        fanout.clone(),
    );
    let receipt = PostReceipt::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        get,
        clock,
        fanout,
    );
    (store, notification.id, silence, receipt)
}

#[tokio::test]
async fn a_key_cannot_silence_its_owner() {
    // The escalation: a leaked key mutes the incident its owner is paged for,
    // the same harm do-not-disturb already refuses a key.
    let alice = user();
    let key = api_key(
        alice.clone(),
        vec![Scope::Publish, Scope::Subscribe],
        vec!["ci-infra"],
    );
    let (_, id, silence, _) = alice_reading_one_notification(&alice).await;

    let refused = silence
        .execute(&key, id, SilenceScope::Notification, None)
        .await
        .expect_err("a key silenced its owner");
    assert_eq!(refused.code(), Some("credential.session_required"));

    assert!(
        silence
            .execute(&Caller::member(alice), id, SilenceScope::Notification, None)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_key_cannot_post_receipts_as_its_owners_phone() {
    // The escalation: a leaked key marks everything "delivered" and "seen" on
    // the owner's phone, so the readership panel says they saw what they never
    // got. A key manages only stream devices; it speaks only for those.
    let alice = user();
    let session = Caller::member(alice.clone());
    let key = api_key(
        alice.clone(),
        vec![Scope::Publish, Scope::Subscribe],
        vec!["ci-infra"],
    );
    let (store, id, _, receipt) = alice_reading_one_notification(&alice).await;
    let enrol = EnrolDevice::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(TestClock::new()),
    );
    let phone = enrol
        .execute(&session, "phone", webpush(), None)
        .await
        .expect("a session enrols a push device");
    let cli = enrol
        .execute(&key, "cli", PushTransport::Stream, None)
        .await
        .expect("a key enrols a stream device");

    let refused = receipt
        .execute(&key, id, ReceiptKind::Seen, Some(phone.id))
        .await
        .expect_err("a key posted a receipt for its owner's phone");
    assert_eq!(refused.code(), Some("credential.session_required"));

    assert!(
        receipt
            .execute(&key, id, ReceiptKind::Seen, Some(cli.id))
            .await
            .is_ok(),
        "the CLI's own stream device is still a key's to speak for"
    );
    assert!(
        receipt
            .execute(&session, id, ReceiptKind::Seen, Some(phone.id))
            .await
            .is_ok()
    );
}

// ── The avatar route ─────────────────────────────────────────────────────────

#[tokio::test]
async fn the_avatar_route_does_not_serve_an_attachment() {
    // The escalation: avatars and attachments share one object store, and
    // `/avatars/{id}` does no channel check — so it must not resolve an id that is
    // an attachment in a channel the caller cannot read.
    let store = InMemory::new();
    let attachments = InMemoryAttachments::new();
    let secret = AttachmentId::generate();
    attachments
        .put(secret, "application/pdf", b"payroll".to_vec())
        .await
        .expect("stored");

    let avatar = AttachmentId::generate();
    attachments
        .put(avatar, "image/png", b"png".to_vec())
        .await
        .expect("stored");
    let mut alice = User::from_claims(
        UserId::generate(),
        "alice",
        None,
        None,
        TestClock::new().now(),
    )
    .expect("a user");
    alice.profile.avatar = Some(avatar);
    store.seed_user(alice);

    let read = ReadAvatar::new(Arc::new(store), Arc::new(attachments));
    let mallory = Caller::member(user());

    assert!(
        read.execute(&mallory, secret)
            .await
            .expect("a lookup")
            .is_none(),
        "the avatar route served a channel attachment"
    );
    assert!(
        read.execute(&mallory, avatar)
            .await
            .expect("a lookup")
            .is_some()
    );
}
