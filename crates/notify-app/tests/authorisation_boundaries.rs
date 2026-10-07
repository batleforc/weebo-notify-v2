//! Authorisation, written as attacks (SEC-P2-38): each test names an escalation and
//! asserts it is refused, so a refactor that widens a grant fails here.

#![allow(
    clippy::expect_used,
    reason = "a failing expect in a test is a failing test, and the message is the diagnosis"
)]

use std::sync::Arc;

use notify_app::{Authorizer, Caller, Grant};
use notify_domain::{
    ApiKeyId, Channel, ChannelId, Membership, Principal, Scope, ServiceAccountId, UserId,
};
use notify_ports::test_utils::InMemory;

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
