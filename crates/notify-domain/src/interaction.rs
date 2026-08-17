//! What comes back from a notification (design §2.3).
//!
//! `ack`, `reply` and `choice` are recorded on the server and read back by
//! whichever service is waiting on them — the script blocked on its `approve`, the
//! deploy pipeline waiting for a human. No device ever calls out to a third party
//! on a notification's behalf, which is what keeps a notification from being a way
//! to make somebody else's browser do work.
//!
//! Two deduplication rules, both scoped to *(principal, action key)*:
//!
//! * **`ack` is idempotent.** Clicking twice records once. A retrying client, a
//!   double tap on a phone and a notification mirrored on three devices all
//!   converge on the same single acknowledgement.
//! * **`choice` is exclusive, last write wins.** Changing your mind replaces the
//!   previous pick rather than adding to it — the same arbitration rule as the
//!   notification state machine (design §2.2).
//!
//! `reply` deduplicates nothing: replies are comments, and the design gives them
//! no key to be unique on. They accumulate in arrival order.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::action::{Action, ActionKind, Actions};
use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, required_text};
use crate::ids::{ActionKey, ChoiceOptionKey, InteractionId, NotificationId};
use crate::limits::MAX_REPLY_CHARS;
use crate::principal::{Author, Principal};

/// What a user sent back.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Interaction {
    /// Acknowledgement of the action registered under `key`.
    Ack {
        /// The action acknowledged.
        key: ActionKey,
    },
    /// Free text. Unkeyed, because the design gives `reply` actions no key.
    Reply {
        /// The text, sanitised like any other user content.
        text: String,
    },
    /// One option of the `choice` action registered under `key`.
    Choice {
        /// The action answered.
        key: ActionKey,
        /// The option picked.
        option: ChoiceOptionKey,
    },
}

impl Interaction {
    /// Which action kind this interaction belongs to.
    #[must_use]
    pub const fn kind(&self) -> ActionKind {
        match self {
            Self::Ack { .. } => ActionKind::Ack,
            Self::Reply { .. } => ActionKind::Reply,
            Self::Choice { .. } => ActionKind::Choice,
        }
    }

    /// The action key, for the two kinds that have one.
    #[must_use]
    pub const fn key(&self) -> Option<&ActionKey> {
        match self {
            Self::Ack { key } | Self::Choice { key, .. } => Some(key),
            Self::Reply { .. } => None,
        }
    }

    /// Check this interaction against the action set of the current revision, and
    /// sanitise whatever text it carries.
    ///
    /// Actions are replaced wholesale by every revision (design §2.3), so an
    /// interaction arriving against an action that has since disappeared is an
    /// ordinary race, not an attack — it is refused with a specific code so a
    /// client can say "this button is gone" rather than "something failed".
    pub fn validate(self, actions: &Actions) -> Result<Self> {
        match self {
            Self::Ack { key } => {
                expect_kind(actions, &key, ActionKind::Ack)?;
                Ok(Self::Ack { key })
            }
            Self::Reply { text } => {
                if !actions
                    .as_slice()
                    .iter()
                    .any(|action| action.kind() == ActionKind::Reply)
                {
                    return Err(DomainError::UnknownActionKey {
                        key: "reply".to_owned(),
                    });
                }
                Ok(Self::Reply {
                    text: required_text(
                        &text,
                        TextKind::MultiLine,
                        MAX_REPLY_CHARS,
                        "interaction.text",
                    )?,
                })
            }
            Self::Choice { key, option } => {
                let action = expect_kind(actions, &key, ActionKind::Choice)?;
                let Action::Choice { options, .. } = action else {
                    // Unreachable: `expect_kind` just established the kind.
                    return Err(DomainError::ActionKindMismatch {
                        key: key.to_string(),
                        attempted: ActionKind::Choice.as_str(),
                    });
                };
                if !options.iter().any(|candidate| candidate.key == option) {
                    return Err(DomainError::UnknownChoiceOption {
                        key: key.to_string(),
                        option: option.to_string(),
                    });
                }
                Ok(Self::Choice { key, option })
            }
        }
    }
}

fn expect_kind<'a>(
    actions: &'a Actions,
    key: &ActionKey,
    expected: ActionKind,
) -> Result<&'a Action> {
    let action = actions
        .find(key)
        .ok_or_else(|| DomainError::UnknownActionKey {
            key: key.to_string(),
        })?;
    if action.kind() == expected {
        Ok(action)
    } else {
        Err(DomainError::ActionKindMismatch {
            key: key.to_string(),
            attempted: expected.as_str(),
        })
    }
}

/// A stored interaction.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionRecord {
    /// Identity.
    pub id: InteractionId,
    /// The notification it answers.
    pub notification_id: NotificationId,
    /// Who answered — shown in the timeline and in the stream (design §8.1).
    pub author: Author,
    /// What they answered.
    pub interaction: Interaction,
    /// When the server recorded it.
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
}

/// What recording an interaction actually did.
///
/// The caller needs the distinction: only `Created` and `Replaced` are worth
/// broadcasting on the stream, and only those two are worth writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    /// First time — store it and broadcast.
    Created(InteractionRecord),
    /// An identical acknowledgement already existed. Nothing to do.
    Unchanged(InteractionId),
    /// A previous pick was superseded (`choice` only).
    Replaced {
        /// The pick that now stands.
        record: InteractionRecord,
        /// The pick it displaced.
        previous: InteractionId,
    },
}

impl Recorded {
    /// The interaction that now stands, if this changed anything.
    #[must_use]
    pub const fn record(&self) -> Option<&InteractionRecord> {
        match self {
            Self::Created(record) | Self::Replaced { record, .. } => Some(record),
            Self::Unchanged(_) => None,
        }
    }
}

/// The interactions recorded against one notification.
///
/// Held as a plain collection so the deduplication rules are testable without a
/// database; the persistence adapter enforces the same uniqueness with an index,
/// because two concurrent acknowledgements must not both win a race the domain
/// cannot see.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InteractionLog {
    records: Vec<InteractionRecord>,
}

impl InteractionLog {
    /// Rehydrate from storage.
    #[must_use]
    pub fn from_records(records: Vec<InteractionRecord>) -> Self {
        Self { records }
    }

    /// Everything recorded, in arrival order.
    #[must_use]
    pub fn records(&self) -> &[InteractionRecord] {
        &self.records
    }

    /// Validate an interaction against the current action set and apply the
    /// deduplication rules.
    pub fn record(
        &mut self,
        actions: &Actions,
        notification_id: NotificationId,
        author: Author,
        interaction: Interaction,
        now: OffsetDateTime,
    ) -> Result<Recorded> {
        let interaction = interaction.validate(actions)?;

        let existing = self
            .records
            .iter()
            .position(|record| is_same_slot(record, &author.principal, &interaction));

        let record = InteractionRecord {
            id: InteractionId::generate(),
            notification_id,
            author,
            interaction,
            recorded_at: now,
        };

        match existing {
            // Acknowledging twice is the same acknowledgement.
            Some(index) if record.interaction.kind() == ActionKind::Ack => {
                Ok(Recorded::Unchanged(self.records[index].id))
            }
            // Changing your mind replaces the pick.
            Some(index) => {
                let previous = self.records[index].id;
                self.records[index] = record.clone();
                Ok(Recorded::Replaced { record, previous })
            }
            None => {
                self.records.push(record.clone());
                Ok(Recorded::Created(record))
            }
        }
    }

    /// The option a principal picked for a given `choice` action, if any.
    #[must_use]
    pub fn choice_of(&self, principal: &Principal, key: &ActionKey) -> Option<&ChoiceOptionKey> {
        self.records
            .iter()
            .find_map(|record| match &record.interaction {
                Interaction::Choice {
                    key: recorded_key,
                    option,
                } if recorded_key == key && &record.author.principal == principal => Some(option),
                _ => None,
            })
    }

    /// Whether a principal has acknowledged a given key.
    #[must_use]
    pub fn has_acked(&self, principal: &Principal, key: &ActionKey) -> bool {
        self.records.iter().any(|record| {
            matches!(&record.interaction, Interaction::Ack { key: recorded } if recorded == key)
                && &record.author.principal == principal
        })
    }
}

/// Whether two interactions occupy the same "one per user per key" slot. Replies
/// never do — they have no key and are meant to accumulate.
fn is_same_slot(
    record: &InteractionRecord,
    principal: &Principal,
    candidate: &Interaction,
) -> bool {
    if &record.author.principal != principal {
        return false;
    }
    match (&record.interaction, candidate) {
        (Interaction::Ack { key: recorded }, Interaction::Ack { key })
        | (Interaction::Choice { key: recorded, .. }, Interaction::Choice { key, .. }) => {
            recorded == key
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;
    use crate::action::{ActionDraft, ChoiceOptionDraft};
    use crate::ids::UserId;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap()
    }

    fn author(principal: &Principal) -> Author {
        Author::anonymous(principal.clone())
    }

    fn user() -> Principal {
        Principal::User {
            id: UserId::generate(),
        }
    }

    fn key(raw: &str) -> ActionKey {
        ActionKey::parse(raw).unwrap()
    }

    fn actions() -> Actions {
        Actions::new(vec![
            ActionDraft::Ack {
                label: "Ack".to_owned(),
                key: "ack-incident".to_owned(),
            }
            .try_into()
            .unwrap(),
            ActionDraft::Reply {
                label: "Comment".to_owned(),
                placeholder: None,
            }
            .try_into()
            .unwrap(),
            ActionDraft::Choice {
                label: "Escalade".to_owned(),
                key: "escalate".to_owned(),
                options: vec![
                    ChoiceOptionDraft {
                        key: "oncall".to_owned(),
                        label: "Page on-call".to_owned(),
                    },
                    ChoiceOptionDraft {
                        key: "defer".to_owned(),
                        label: "Attendre".to_owned(),
                    },
                ],
            }
            .try_into()
            .unwrap(),
        ])
        .unwrap()
    }

    fn log() -> (InteractionLog, NotificationId) {
        (InteractionLog::default(), NotificationId::generate())
    }

    // --- T4 · Interactions (design §3.4) ----------------------------------

    #[test]
    fn critical_t4_acknowledging_twice_records_once() {
        let (mut log, id) = log();
        let alice = user();

        let first = log
            .record(
                &actions(),
                id,
                author(&alice),
                Interaction::Ack {
                    key: key("ack-incident"),
                },
                now(),
            )
            .unwrap();
        assert!(matches!(first, Recorded::Created(_)));

        let second = log
            .record(
                &actions(),
                id,
                author(&alice),
                Interaction::Ack {
                    key: key("ack-incident"),
                },
                now(),
            )
            .unwrap();
        assert!(
            matches!(second, Recorded::Unchanged(_)),
            "a retrying client must not double-acknowledge"
        );
        assert_eq!(log.records().len(), 1);
        assert!(log.has_acked(&alice, &key("ack-incident")));
    }

    #[test]
    fn critical_t4_acknowledgements_are_per_user() {
        let (mut log, id) = log();
        let (alice, bob) = (user(), user());
        let ack = || Interaction::Ack {
            key: key("ack-incident"),
        };

        log.record(&actions(), id, author(&alice), ack(), now())
            .unwrap();
        log.record(&actions(), id, author(&bob), ack(), now())
            .unwrap();

        assert_eq!(
            log.records().len(),
            2,
            "Bob acknowledging is not Alice acknowledging"
        );
    }

    #[test]
    fn critical_t4_a_choice_is_exclusive_and_the_last_one_wins() {
        let (mut log, id) = log();
        let alice = user();
        let pick = |option: &str| Interaction::Choice {
            key: key("escalate"),
            option: ChoiceOptionKey::parse(option).unwrap(),
        };

        log.record(&actions(), id, author(&alice), pick("defer"), now())
            .unwrap();
        let changed = log
            .record(&actions(), id, author(&alice), pick("oncall"), now())
            .unwrap();

        assert!(matches!(changed, Recorded::Replaced { .. }));
        assert_eq!(log.records().len(), 1);
        assert_eq!(
            log.choice_of(&alice, &key("escalate")).unwrap().as_str(),
            "oncall"
        );
    }

    #[test]
    fn critical_t4_an_interaction_must_match_a_current_action() {
        let (mut log, id) = log();
        let alice = user();

        // The key exists, but it is a choice, not an acknowledgement.
        let mismatched = log.record(
            &actions(),
            id,
            author(&alice),
            Interaction::Ack {
                key: key("escalate"),
            },
            now(),
        );
        assert_eq!(mismatched.unwrap_err().code(), "action.kind_mismatch");

        // A revision replaced the actions and this button is gone.
        let vanished = log.record(
            &actions(),
            id,
            author(&alice),
            Interaction::Ack {
                key: key("ack-something-else"),
            },
            now(),
        );
        assert_eq!(vanished.unwrap_err().code(), "action.unknown_key");

        let invented = log.record(
            &actions(),
            id,
            author(&alice),
            Interaction::Choice {
                key: key("escalate"),
                option: ChoiceOptionKey::parse("delete-prod").unwrap(),
            },
            now(),
        );
        assert_eq!(invented.unwrap_err().code(), "action.unknown_option");
    }

    #[test]
    fn replies_accumulate_and_are_sanitised() {
        let (mut log, id) = log();
        let alice = user();
        let reply = |text: &str| Interaction::Reply {
            text: text.to_owned(),
        };

        log.record(
            &actions(),
            id,
            author(&alice),
            reply("looking into it"),
            now(),
        )
        .unwrap();
        let second = log
            .record(
                &actions(),
                id,
                author(&alice),
                reply("\u{1b}[2Jetcd is back"),
                now(),
            )
            .unwrap();

        assert_eq!(log.records().len(), 2, "a reply is a comment, not a vote");
        assert_eq!(
            second.record().unwrap().interaction,
            Interaction::Reply {
                text: "etcd is back".to_owned()
            }
        );
    }

    #[test]
    fn a_reply_needs_a_reply_action_to_exist() {
        let (mut log, id) = log();
        let no_reply_button = Actions::new(vec![
            ActionDraft::Ack {
                label: "Ack".to_owned(),
                key: "ack-incident".to_owned(),
            }
            .try_into()
            .unwrap(),
        ])
        .unwrap();

        let refused = log.record(
            &no_reply_button,
            id,
            author(&user()),
            Interaction::Reply {
                text: "hello".to_owned(),
            },
            now(),
        );
        assert_eq!(refused.unwrap_err().code(), "action.unknown_key");
    }
}
