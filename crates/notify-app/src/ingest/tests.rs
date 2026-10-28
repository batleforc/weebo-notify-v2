use std::sync::atomic::{AtomicUsize, Ordering};

use notify_domain::{Channel, Membership, Principal, Priority, UserId};
use notify_ports::NoMetrics;
use notify_ports::test_utils::{InMemory, TestClock, TestRateLimiter};

use super::*;
use crate::attribution::Attribution;
use crate::fanout::Fanout;

/// A transform that answers fixed elements and counts its evaluations.
struct Fixed {
    id: String,
    channels: Vec<ChannelId>,
    update: bool,
    raw: RawMode,
    lang: FeedLang,
    elements: Vec<Mapped>,
    evaluations: AtomicUsize,
}

impl Fixed {
    fn new(elements: Vec<Mapped>) -> Self {
        Self {
            id: "mlab-news".to_owned(),
            channels: vec![channel()],
            update: false,
            raw: RawMode::Off,
            lang: FeedLang::En,
            elements,
            evaluations: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Transform for Fixed {
    fn id(&self) -> &str {
        &self.id
    }
    fn channels(&self) -> &[ChannelId] {
        &self.channels
    }
    fn update(&self) -> bool {
        self.update
    }
    fn raw(&self) -> RawMode {
        self.raw
    }
    fn lang(&self) -> FeedLang {
        self.lang
    }
    fn title_fallback(&self) -> &'static str {
        "mlab news"
    }
    async fn evaluate(&self, _body: Vec<u8>) -> Result<Vec<Mapped>, TransformError> {
        self.evaluations.fetch_add(1, Ordering::SeqCst);
        Ok(self.elements.clone())
    }
}

struct Registry {
    transforms: Vec<Arc<Fixed>>,
}

impl TransformRegistry for Registry {
    fn get(&self, id: &str) -> Option<Arc<dyn Transform>> {
        self.transforms
            .iter()
            .find(|transform| transform.id == id)
            .map(|transform| Arc::clone(transform) as Arc<dyn Transform>)
    }
}

fn channel() -> ChannelId {
    ChannelId::parse("security").unwrap()
}

fn titled(title: &str, id: &str) -> Mapped {
    Mapped {
        title: Some(title.to_owned()),
        external_id: Some(id.to_owned()),
        ..Mapped::default()
    }
}

struct Fixture {
    ingest: IngestJson,
    transform: Arc<Fixed>,
    alice: Caller,
    stranger: Caller,
    limiter: TestRateLimiter,
}

fn fixture(transform: Fixed) -> Fixture {
    let clock = TestClock::new();
    let store = InMemory::with_clock(clock.clone());
    let limiter = TestRateLimiter::new();
    let alice = Principal::User {
        id: UserId::generate(),
    };
    let mut seeded = Channel::dynamic(channel(), None).unwrap();
    seeded.upsert_member(Membership::new(alice.clone(), vec![Scope::Publish]).unwrap());
    store.seed_channel(seeded);
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
    let upsert = UpsertNotification::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(limiter.clone()),
        Arc::new(clock),
        authorizer.clone(),
        fanout,
        Attribution::new(Arc::new(store.clone()), Arc::new(store)),
    );
    let transform = Arc::new(transform);
    let registry = Arc::new(Registry {
        transforms: vec![Arc::clone(&transform)],
    });
    Fixture {
        ingest: IngestJson::new(authorizer, upsert, registry, Arc::new(NoMetrics)),
        transform,
        alice: Caller::member(alice),
        stranger: Caller::member(Principal::User {
            id: UserId::generate(),
        }),
        limiter,
    }
}

#[tokio::test]
async fn publish_is_checked_before_anything_is_evaluated() {
    let fixture = fixture(Fixed::new(vec![titled("a", "1")]));

    let refused = fixture
        .ingest
        .execute(&fixture.stranger, "mlab-news", channel(), b"{}".to_vec())
        .await;

    assert!(matches!(
        refused,
        Err(IngestError::Refused(Failure::Domain(_)))
    ));
    assert_eq!(fixture.transform.evaluations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_unknown_transform_and_an_unlisted_channel_are_one_404_before_evaluation() {
    let mut transform = Fixed::new(vec![titled("a", "1")]);
    transform.channels = vec![ChannelId::parse("veille").unwrap()];
    let fixture = fixture(transform);

    for id in ["nope", "mlab-news"] {
        let answer = fixture
            .ingest
            .execute(&fixture.alice, id, channel(), b"{}".to_vec())
            .await;
        assert!(matches!(answer, Err(IngestError::NotFound)), "{id}");
    }
    assert_eq!(fixture.transform.evaluations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_repeated_element_is_skipped_without_spending_quota() {
    let fixture = fixture(Fixed::new(vec![titled("a", "1"), titled("b", "2")]));
    let run = || {
        fixture
            .ingest
            .execute(&fixture.alice, "mlab-news", channel(), b"{}".to_vec())
    };

    let first = run().await.unwrap();
    let checks = fixture.limiter.seen().len();
    let second = run().await.unwrap();

    assert!(first.iter().all(|e| matches!(e, Upserted::Written(_))));
    assert!(second.iter().all(|e| matches!(e, Upserted::Skipped(_))));
    assert_eq!(fixture.limiter.seen().len(), checks);
}

#[tokio::test]
async fn a_refused_element_is_skipped_and_all_refused_is_a_422() {
    // Within the body's character ceiling, past the payload's byte ceiling.
    let too_big = Mapped {
        body: Some("😀".repeat(MAX_BODY_CHARS)),
        ..titled("too big", "big")
    };
    let mixed = fixture(Fixed::new(vec![too_big.clone(), titled("ok", "ok")]));
    let published = mixed
        .ingest
        .execute(&mixed.alice, "mlab-news", channel(), b"{}".to_vec())
        .await
        .unwrap();
    assert_eq!(published.len(), 1);

    let all = fixture(Fixed::new(vec![too_big]));
    let answer = all
        .ingest
        .execute(&all.alice, "mlab-news", channel(), b"{}".to_vec())
        .await;
    assert!(
        matches!(answer, Err(IngestError::Refused(Failure::Domain(_)))),
        "{answer:?}"
    );
}

#[test]
fn the_reserved_prefixes_are_the_declared_ids_and_a_colon() {
    let fixture = fixture(Fixed::new(Vec::new()));

    assert!(fixture.ingest.reserves("mlab-news:x"));
    assert!(!fixture.ingest.reserves("mlab-newsy:x"));
    assert!(!fixture.ingest.reserves("mlab-news"));
}

#[test]
fn the_title_falls_back_and_every_string_is_cut_at_a_character() {
    let transform = Fixed::new(Vec::new());
    let (draft, _) = transform_draft(&Mapped::default(), &transform);
    assert_eq!(draft.content.title, "mlab news");

    let (draft, _) = transform_draft(&titled("  \n ", "1"), &transform);
    assert_eq!(draft.content.title, "mlab news");

    let mapped = Mapped {
        group_key: Some("é".repeat(500)),
        tags: vec![String::new(), "a".repeat(40), "news".to_owned()],
        priority: Some(Priority::High),
        ..titled(&"é".repeat(500), "1")
    };
    let (draft, _) = transform_draft(&mapped, &transform);
    assert_eq!(draft.content.title.chars().count(), MAX_TITLE_CHARS);
    assert_eq!(
        draft.content.group_key.unwrap().chars().count(),
        MAX_GROUP_KEY_CHARS
    );
    assert_eq!(draft.content.tags, ["news"]);
    assert_eq!(draft.priority, Some(Priority::High));
}

#[test]
fn a_link_becomes_view_and_ack_follows_it() {
    let transform = Fixed {
        lang: FeedLang::Fr,
        ..Fixed::new(Vec::new())
    };
    let mapped = Mapped {
        link: Some("https://news.mlab.sh/a/1".to_owned()),
        ack: Some("V".repeat(60)),
        ..titled("t", "1")
    };
    let (draft, _) = transform_draft(&mapped, &transform);
    let actions = draft.content.actions.as_slice();
    assert!(matches!(&actions[0], Action::View { label, .. } if label == "Ouvrir"));
    assert!(
        matches!(&actions[1], Action::Ack { label, key } if label.chars().count() == MAX_LABEL_CHARS && key.to_string() == "ack")
    );

    let javascript = Mapped {
        link: Some("javascript:alert(1)".to_owned()),
        ..titled("t", "1")
    };
    let (draft, _) = transform_draft(&javascript, &transform);
    assert!(draft.content.actions.is_empty());
}

#[test]
fn the_external_id_is_prefixed_or_digested() {
    let (id, digested) = external_id("mlab-news", &titled("t", "n1")).unwrap();
    assert_eq!((id.as_str(), digested), ("mlab-news:n1", false));

    let too_long = titled("t", &"x".repeat(200));
    let (id, digested) = external_id("mlab-news", &too_long).unwrap();
    assert!(digested && id.as_str().starts_with("mlab-news:"));

    let element = |json: &str| Mapped {
        element: serde_json::from_str(json).unwrap(),
        ..Mapped::default()
    };
    let a = external_id("t", &element(r#"{"a":1,"b":2}"#)).unwrap();
    let b = external_id("t", &element(r#"{"b":2,"a":1}"#)).unwrap();
    assert_eq!(a, b);
    assert!(a.1);
}

#[test]
fn the_fence_is_one_backtick_longer_than_any_run_in_the_json() {
    for (text, fence) in [
        ("plain", "```json"),
        ("a```b", "````json"),
        ("a``````````b", "```````````json"),
    ] {
        let (body, cut) = with_raw(
            Some("intro"),
            &serde_json::json!({ "k": text }),
            FeedLang::En,
        );
        assert!(!cut);
        assert!(body.starts_with("intro\n\n"), "{body}");
        assert!(body.contains(&format!("{fence}\n")), "{body}");
        assert!(
            body.ends_with(&format!("\n{}", fence.trim_end_matches("json"))),
            "{body}"
        );
    }
}

#[test]
fn a_raw_block_too_long_is_cut_closed_and_marked_in_the_transforms_language() {
    let json = serde_json::json!({ "k": "é".repeat(20_000) });

    let (body, cut) = with_raw(None, &json, FeedLang::Fr);

    assert!(cut);
    assert!(body.chars().count() <= MAX_BODY_CHARS);
    assert!(
        body.ends_with("```\n\n_(tronqué)_"),
        "{}",
        &body[body.len() - 40..]
    );
}

#[test]
fn a_body_already_at_the_ceiling_still_leaves_the_whole_in_bounds() {
    let json = serde_json::json!({ "k": "v" });

    let (body, cut) = with_raw(Some(&"b".repeat(MAX_BODY_CHARS)), &json, FeedLang::En);

    assert!(cut);
    assert!(
        body.chars().count() <= MAX_BODY_CHARS,
        "{}",
        body.chars().count()
    );
    assert!(body.ends_with("```\n\n_(truncated)_"));
}
