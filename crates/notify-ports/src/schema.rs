//! The two OpenAPI schemas this crate writes by hand (design §14.4, D1 and D3).
//!
//! Everywhere else the document is a derive over the serde representation
//! (ADR-0013): the type is the source of truth and nobody maintains a second
//! description of it. These two are the exceptions, and both for the same reason —
//! what utoipa *composes* for them is valid OpenAPI that no generator can model,
//! and ADR-0009 promises the clients come from the contract. A contract a generator
//! chokes on is a promise with a hole in it.
//!
//! # D1 — [`Event`]
//!
//! [`Event`] flattens an audience and a payload into one object, which is the flat
//! wire shape design §8.1 documents. Derived, that comes out as
//! `allOf[oneOf, oneOf, object]`, and progenitor renders it as `pub enum
//! EventVariant0 {}` — an **uninhabited** type, so a generated client can
//! deserialise a page of `GET /sync` only for as long as it contains no events.
//!
//! Written by hand it is a `oneOf` of the five events that actually exist, each one
//! complete: its own `event` discriminant, the audience it is really addressed to,
//! and the envelope. **The bytes on the wire do not change** — only their
//! description does, into something a generator turns into five struct variants.
//!
//! The payload half of each branch stays a `$ref` to the schema derived from the
//! payload type, rather than a hand-copied property list: an `allOf` of a `$ref`
//! and an object is a shape generators *do* merge — it is what `EventPayload`
//! already compiled to — so adding a field to [`RevisionEvent`] still updates the
//! document by itself. What is written by hand is the envelope, five short objects,
//! and the test below holds it to the type by round-tripping a real event of each
//! kind through it.
//!
//! # D3 — [`Page<T>`]
//!
//! utoipa inlines a generic parameter, so `Page_Channel.items` described a whole
//! channel instead of referencing `Channel`. Structural typing means the TS client
//! never noticed; progenitor mints a second nominal type — `PageChannelItemsItem` —
//! for something the document already names. Written by hand, `items` is an array of
//! **references**, and the document loses a full copy of `Channel`, `Notification`
//! and `Revision` along the way.
//!
//! [`RevisionEvent`]: crate::event::RevisionEvent

use std::borrow::Cow;

use notify_domain::{
    ChannelId, Cursor, InteractionRecord, NotificationId, Receipt, TombstoneReason, UserId,
};
use utoipa::openapi::{
    AllOfBuilder, Array, KnownFormat, Object, ObjectBuilder, OneOf, Ref, RefOr, Schema,
    SchemaFormat, Type,
};
use utoipa::{PartialSchema, ToSchema};

use crate::event::{Event, PreferenceEvent, RevisionEvent};
use crate::pagination::Page;

// --- Event (D1) -----------------------------------------------------------

/// Who an event is addressed to.
///
/// Not a free choice per kind: a `preference` event is one user's own business and
/// every other kind belongs to a channel (scenario T26). Saying so in the schema is
/// what lets a generated client read `channel_id` off a revision without unwrapping
/// an option that can never be empty.
#[derive(Clone, Copy)]
enum Reach {
    Channel,
    User,
}

impl PartialSchema for Event {
    fn schema() -> RefOr<Schema> {
        OneOf::builder()
            .description(Some(
                "One row of the event log, flattened as design §8.1 puts it on the \
                 wire: the envelope, the audience and the payload share one object.",
            ))
            .item(carrying::<RevisionEvent>("revision", Reach::Channel))
            .item(carrying::<Receipt>("receipt", Reach::Channel))
            .item(carrying::<InteractionRecord>("interaction", Reach::Channel))
            .item(tombstone())
            .item(closure())
            .item(carrying::<PreferenceEvent>("preference", Reach::User))
            .into()
    }
}

impl ToSchema for Event {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("Event")
    }

    fn schemas(schemas: &mut Vec<(String, RefOr<Schema>)>) {
        // Everything the six branches reference, directly or through a payload. A
        // `$ref` to a schema nobody registered is a document that does not resolve,
        // and the generators report that in a way nobody enjoys reading.
        register::<Cursor>(schemas);
        register::<ChannelId>(schemas);
        register::<UserId>(schemas);
        register::<NotificationId>(schemas);
        register::<TombstoneReason>(schemas);
        register::<RevisionEvent>(schemas);
        register::<Receipt>(schemas);
        register::<InteractionRecord>(schemas);
        register::<PreferenceEvent>(schemas);
    }
}

/// One branch: the envelope of an event of `kind`, plus the payload type it carries.
fn carrying<T: ToSchema>(kind: &str, reach: Reach) -> AllOfBuilder {
    AllOfBuilder::new()
        .title(Some(branch_title(kind)))
        .item(Ref::from_schema_name(T::name()))
        .item(envelope(kind, reach))
}

/// The title a branch is known by.
///
/// Not decoration: a generator names the variant it mints after it, so without one
/// the five events come out as `Variant0`…`Variant4` — and a client matching on
/// positions silently changes meaning the day a branch moves.
fn branch_title(kind: &str) -> String {
    let mut title = String::from("Logged");
    let mut characters = kind.chars();
    if let Some(first) = characters.next() {
        title.extend(first.to_uppercase());
        title.push_str(characters.as_str());
    }
    title
}

/// The tombstone branch, which carries no payload type of its own — "gone, and why"
/// is two fields, and giving them a struct would name something nothing else reads.
fn tombstone() -> ObjectBuilder {
    envelope("tombstone", Reach::Channel)
        .title(Some(branch_title("tombstone")))
        .description(Some(
            "A notification stopped existing; clients drop it locally.",
        ))
        .property(
            "notification_id",
            Ref::from_schema_name(NotificationId::name()),
        )
        .required("notification_id")
        .property("reason", Ref::from_schema_name(TombstoneReason::name()))
        .required("reason")
}

/// The closure branch. Same reasoning as [`tombstone`] for having no payload type:
/// "this one closed, at this instant" is two fields.
///
/// Distinct from a tombstone on the wire because it means the opposite thing to a
/// client — the notification is still there, and still readable, it has only
/// stopped being open. A client that treated this as a tombstone would make a
/// closed incident disappear from the inbox that is supposed to keep it until its
/// TTL runs out.
fn closure() -> ObjectBuilder {
    envelope("closure", Reach::Channel)
        .title(Some(branch_title("closure")))
        .description(Some(
            "A lifecycle notification was closed by the server because nothing had \
             happened to it for longer than its channel allows; clients mark their \
             copy closed rather than dropping it.",
        ))
        .property(
            "notification_id",
            Ref::from_schema_name(NotificationId::name()),
        )
        .required("notification_id")
        .property(
            "closed_at",
            Object::builder()
                .schema_type(Type::String)
                .format(Some(utoipa::openapi::SchemaFormat::KnownFormat(
                    utoipa::openapi::KnownFormat::DateTime,
                )))
                .description(Some(
                    "When the server closed it, and therefore when its retention \
                     clock starts.",
                )),
        )
        .required("closed_at")
}

/// What every event carries whatever happened: where it sits in the log, when it was
/// appended, who it is for, and which kind it is.
fn envelope(kind: &str, reach: Reach) -> ObjectBuilder {
    let common = Object::builder()
        .schema_type(Type::Object)
        .property("cursor", Ref::from_schema_name(Cursor::name()))
        .required("cursor")
        .property(
            "logged_at",
            timestamp("When it was appended, by the server's clock."),
        )
        .required("logged_at")
        // Optional, and that is the contract: it is a fact about *this delivery*
        // to *this reader*, absent when false, and a client written before it
        // existed keeps parsing (design §7.4, §14.4 D22).
        .property(
            "muted_by_dnd",
            Object::builder()
                .schema_type(Type::Boolean)
                .description(Some(
                    "Whether the reader's do-not-disturb was running when this \
                     happened. Set per delivery, never stored: a client uses it to \
                     suppress its own popup while the inbox fills as usual.",
                ))
                .build(),
        )
        .property("event", constant(kind))
        .required("event");

    match reach {
        Reach::Channel => common
            .property("audience", constant("channel"))
            .required("audience")
            .property("channel_id", Ref::from_schema_name(ChannelId::name()))
            .required("channel_id"),
        Reach::User => common
            .property("audience", constant("user"))
            .required("audience")
            .property("user_id", Ref::from_schema_name(UserId::name()))
            .required("user_id"),
    }
}

// --- Page<T> (D3) ---------------------------------------------------------

/// A generic body is composed rather than merely described: `body = Page<Channel>`
/// expands to a call on this trait, and [`PartialSchema`] then comes from utoipa's
/// blanket implementation over it. Hidden from utoipa's docs, but it is the same
/// entry point its own derive targets — and the alternative, a concrete `ChannelPage`
/// newtype per element, would put three empty types in the domain to satisfy a
/// generator.
impl<T: ToSchema> utoipa::__dev::ComposeSchema for Page<T> {
    fn compose(_generics: Vec<RefOr<Schema>>) -> RefOr<Schema> {
        Object::builder()
            .schema_type(Type::Object)
            .description(Some("What a caller gets."))
            .property(
                "items",
                Array::builder()
                    .items(RefOr::Ref(Ref::from_schema_name(T::name())))
                    .description(Some("The rows, in the ordering of the endpoint.")),
            )
            .required("items")
            .property(
                "next",
                OneOf::builder()
                    .item(Object::builder().schema_type(Type::Null))
                    .item(Ref::from_schema_name(Cursor::name()))
                    .description(Some(
                        "Pass this back as the `cursor` of the next request. Absent \
                         means this was the last page. A present cursor does not \
                         promise the next page is non-empty — rows can expire between \
                         two calls — so clients follow it until it is absent rather \
                         than until a page comes back short.",
                    )),
            )
            .into()
    }
}

impl<T: ToSchema> ToSchema for Page<T> {
    /// A *prefix*, not the whole name: utoipa appends the element's own name, so
    /// this is what makes the schema `Page_Channel` — the name its own alias
    /// produced, which the generated clients already spell `PageChannel`.
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("Page")
    }

    fn schemas(schemas: &mut Vec<(String, RefOr<Schema>)>) {
        register::<Cursor>(schemas);
        register::<T>(schemas);
    }
}

// --- Shared helpers -------------------------------------------------------

/// Register `T` and everything it references.
fn register<T: ToSchema>(schemas: &mut Vec<(String, RefOr<Schema>)>) {
    schemas.push((T::name().into(), T::schema()));
    <T as ToSchema>::schemas(schemas);
}

/// An RFC 3339 timestamp.
fn timestamp(description: &str) -> ObjectBuilder {
    Object::builder()
        .schema_type(Type::String)
        .format(Some(SchemaFormat::KnownFormat(KnownFormat::DateTime)))
        .description(Some(description))
}

/// A field with exactly one legal value — the discriminant of a branch.
fn constant(value: &str) -> ObjectBuilder {
    Object::builder()
        .schema_type(Type::String)
        .enum_values(Some([value]))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use notify_domain::{
        Author, ChannelPolicy, ContentDraft, Dnd, Interaction, InteractionId, Notification,
        Principal, ReceiptId, ReceiptKind, RevisionDraft, State,
    };
    use serde_json::Value;
    use time::OffsetDateTime;
    use utoipa::openapi::schema::ArrayItems;

    use super::*;
    use crate::event::{Audience, EventPayload};

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap()
    }

    fn user() -> UserId {
        UserId::generate()
    }

    fn channel() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn revision_event() -> RevisionEvent {
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel(),
            None,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("etcd degraded")),
            Author::anonymous(Principal::User { id: user() }),
            now(),
            &ChannelPolicy::default(),
        )
        .unwrap();

        RevisionEvent::new(&notification, &written.revision)
    }

    /// One real event of each kind, in the order the `oneOf` branches are written.
    fn one_of_each() -> Vec<Event> {
        let payloads = [
            (
                Audience::Channel { id: channel() },
                EventPayload::Revision(Box::new(revision_event())),
            ),
            (
                Audience::Channel { id: channel() },
                EventPayload::Receipt(Receipt {
                    id: ReceiptId::generate(),
                    notification_id: NotificationId::generate(),
                    principal: Principal::User { id: user() },
                    device: None,
                    kind: ReceiptKind::Seen,
                    at: now(),
                }),
            ),
            (
                Audience::Channel { id: channel() },
                EventPayload::Interaction(InteractionRecord {
                    id: InteractionId::generate(),
                    notification_id: NotificationId::generate(),
                    author: Author::anonymous(Principal::User { id: user() }),
                    interaction: Interaction::Reply {
                        text: "on it".to_owned(),
                    },
                    recorded_at: now(),
                }),
            ),
            (
                Audience::Channel { id: channel() },
                EventPayload::Tombstone {
                    notification_id: NotificationId::generate(),
                    reason: TombstoneReason::Expired,
                },
            ),
            (
                Audience::Channel { id: channel() },
                EventPayload::Closure {
                    notification_id: NotificationId::generate(),
                    closed_at: now(),
                },
            ),
            (
                Audience::User { id: user() },
                EventPayload::Preference(Box::new(PreferenceEvent {
                    user: user(),
                    dnd: Dnd::off(user()),
                    silences: Vec::new(),
                })),
            ),
        ];

        payloads
            .into_iter()
            .map(|(audience, payload)| Event {
                cursor: Cursor::from_sequence(1),
                audience,
                recorded_at: now(),
                muted_by_dnd: false,
                payload,
            })
            .collect()
    }

    /// The `required` names and the property names of one branch, with its `$ref`
    /// resolved against the payload schemas a branch can name.
    fn flatten(branch: &RefOr<Schema>) -> (Vec<String>, Vec<String>) {
        let mut required = Vec::new();
        let mut properties = Vec::new();

        let mut absorb = |schema: &RefOr<Schema>| {
            if let RefOr::T(Schema::Object(object)) = schema {
                required.extend(object.required.iter().cloned());
                properties.extend(object.properties.keys().cloned());
            }
        };

        match branch {
            RefOr::T(Schema::AllOf(all_of)) => {
                for item in &all_of.items {
                    match item {
                        RefOr::Ref(reference) => absorb(&resolve(&reference.ref_location)),
                        RefOr::T(_) => absorb(item),
                    }
                }
            }
            other => absorb(other),
        }

        (required, properties)
    }

    /// The payload schemas a branch may reference. Hand-listed rather than looked up
    /// in a document, because this test is about *this* schema and not about how the
    /// server assembles the whole one.
    fn resolve(location: &str) -> RefOr<Schema> {
        match location.rsplit('/').next().unwrap_or_default() {
            "RevisionEvent" => RevisionEvent::schema(),
            "Receipt" => Receipt::schema(),
            "InteractionRecord" => InteractionRecord::schema(),
            "PreferenceEvent" => PreferenceEvent::schema(),
            other => panic!("a branch references {other}, which this test cannot resolve"),
        }
    }

    /// What D1 is about, and what decays silently: the envelope is written by hand,
    /// so nothing but a test stops it drifting from the type it describes.
    #[test]
    fn every_kind_of_event_matches_the_branch_that_describes_it() {
        let RefOr::T(Schema::OneOf(one_of)) = Event::schema() else {
            panic!("Event must stay a oneOf — that is the whole of D1");
        };
        let events = one_of_each();
        assert_eq!(
            one_of.items.len(),
            events.len(),
            "a branch per event kind, and an event kind per branch"
        );

        for (event, branch) in events.iter().zip(&one_of.items) {
            let kind = event.kind().as_str();
            let Value::Object(serialised) = serde_json::to_value(event).unwrap() else {
                panic!("an event serialises to an object");
            };
            let (required, properties) = flatten(branch);

            for name in &required {
                assert!(
                    serialised.contains_key(name),
                    "the {kind} branch requires `{name}`, which the event does not carry"
                );
            }
            for name in serialised.keys() {
                assert!(
                    properties.iter().any(|property| property == name),
                    "a {kind} event carries `{name}`, which its branch does not describe"
                );
            }
        }
    }

    #[test]
    fn a_preference_event_is_described_as_the_private_thing_it_is() {
        // Scenario T26 in the document rather than only in the code: a client reading
        // the schema must not expect a channel on it.
        let RefOr::T(Schema::OneOf(one_of)) = Event::schema() else {
            panic!("Event must stay a oneOf");
        };
        let (required, _) = flatten(one_of.items.last().unwrap());

        assert!(required.iter().any(|name| name == "user_id"));
        assert!(!required.iter().any(|name| name == "channel_id"));
    }

    /// D3: the item is a reference, so a generator has one type for a channel rather
    /// than one per container that carries one.
    #[test]
    fn a_page_references_its_item_instead_of_copying_it() {
        use notify_domain::Channel;

        let RefOr::T(Schema::Object(page)) = Page::<Channel>::schema() else {
            panic!("a page is an object");
        };
        let Some(RefOr::T(Schema::Array(items))) = page.properties.get("items") else {
            panic!("`items` is an array");
        };
        let ArrayItems::RefOrSchema(item) = &items.items else {
            panic!("`items` describes its item");
        };

        assert!(
            matches!(item.as_ref(), RefOr::Ref(reference)
                if reference.ref_location.ends_with("/Channel")),
            "the item must be a $ref: inlining it is what mints PageChannelItemsItem"
        );
        // A prefix: utoipa appends the element's name, giving `Page_Channel`. Naming
        // the whole thing here is how it came out `Page_Channel_Channel` once.
        assert_eq!(Page::<Channel>::name(), "Page");
    }
}
