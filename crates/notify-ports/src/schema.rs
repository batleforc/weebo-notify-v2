//! The two hand-written OpenAPI schemas (§14.4, ADR-0009), because the derived ones
//! defeat generators: [`Event`] as a `oneOf` of five complete branches, [`Page<T>`]
//! referencing its items. Wire bytes are unchanged; tests hold both to the types.

use std::borrow::Cow;

use notify_domain::{
    Author, ChannelId, Cursor, InteractionRecord, NotificationId, Receipt, TombstoneReason, UserId,
};
use utoipa::openapi::{
    AllOfBuilder, Array, KnownFormat, Object, ObjectBuilder, OneOf, Ref, RefOr, Schema,
    SchemaFormat, Type,
};
use utoipa::{PartialSchema, ToSchema};

use crate::event::{Event, PreferenceEvent, RevisionEvent};
use crate::pagination::Page;

// --- Event (D1) -----------------------------------------------------------

/// Who an event is addressed to: `preference` events a user, every other kind a channel (T26).
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
        // Everything the branches reference, so every `$ref` resolves.
        register::<Cursor>(schemas);
        register::<ChannelId>(schemas);
        register::<UserId>(schemas);
        register::<NotificationId>(schemas);
        register::<TombstoneReason>(schemas);
        register::<Author>(schemas);
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

/// The title a branch is known by; generators name the variant after it.
fn branch_title(kind: &str) -> String {
    let mut title = String::from("Logged");
    let mut characters = kind.chars();
    if let Some(first) = characters.next() {
        title.extend(first.to_uppercase());
        title.push_str(characters.as_str());
    }
    title
}

/// The tombstone branch: two fields, no payload type.
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

/// The closure branch: two fields, no payload type. Unlike a tombstone, the
/// notification stays readable.
fn closure() -> ObjectBuilder {
    envelope("closure", Reach::Channel)
        .title(Some(branch_title("closure")))
        .description(Some(
            "A lifecycle notification was closed without a revision: by the server \
             because nothing had happened to it for longer than its channel allows, or \
             by a person (`closed_by`); clients mark their copy closed rather than \
             dropping it.",
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
        .property(
            "closed_by",
            OneOf::builder()
                .item(Object::builder().schema_type(Type::Null))
                .item(Ref::from_schema_name(Author::name()))
                .description(Some(
                    "Who closed it by hand (RFC 0004); absent when the server did.",
                )),
        )
}

/// What every event carries: cursor, append time, audience and kind.
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
        // Optional: absent when false, so older clients keep parsing (§7.4).
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

/// `body = Page<Channel>` expands to this trait; [`PartialSchema`] comes from
/// utoipa's blanket impl over it.
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
    /// A prefix: utoipa appends the element's name, giving `Page_Channel`.
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
                    closed_by: None,
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

    /// The payload schemas a branch may reference.
    fn resolve(location: &str) -> RefOr<Schema> {
        match location.rsplit('/').next().unwrap_or_default() {
            "RevisionEvent" => RevisionEvent::schema(),
            "Receipt" => Receipt::schema(),
            "InteractionRecord" => InteractionRecord::schema(),
            "PreferenceEvent" => PreferenceEvent::schema(),
            other => panic!("a branch references {other}, which this test cannot resolve"),
        }
    }

    /// The hand-written envelope must not drift from the type it describes.
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
        // T26: a preference event carries no channel.
        let RefOr::T(Schema::OneOf(one_of)) = Event::schema() else {
            panic!("Event must stay a oneOf");
        };
        let (required, _) = flatten(one_of.items.last().unwrap());

        assert!(required.iter().any(|name| name == "user_id"));
        assert!(!required.iter().any(|name| name == "channel_id"));
    }

    /// The item is a reference, not an inlined copy.
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
        assert_eq!(Page::<Channel>::name(), "Page");
    }
}
