//! Call-to-action buttons carried by a notification (design §2.3).
//!
//! Two properties are worth stating up front because they shape the whole type:
//!
//! * **No action makes an outbound HTTP call from the device.** `ack`, `reply` and
//!   `choice` are recorded on the server and read back by whichever service cares
//!   (design §2.3). Only `view` opens a URL, and only `silence` changes server-side
//!   delivery state. A notification is therefore never a way to make somebody
//!   else's browser talk to a third party.
//! * **`view` URLs are validated at ingestion, not only at click time.** Refusing
//!   `javascript:`, `file:` and custom schemes here means every client — web, TUI,
//!   desktop, mobile — inherits the protection without having to remember it
//!   (scenario T4). Clients check again before opening; belt and braces.
//!
//! Actions are replaced wholesale by every revision (design §2.3), so they carry no
//! identity of their own beyond the publisher-chosen `key` that interactions are
//! addressed to.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::duration::HumanDuration;
use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, optional_text, required_text};
use crate::ids::{ActionKey, ChoiceOptionKey};
use crate::limits::{MAX_ACTIONS, MAX_CHOICE_OPTIONS, MAX_LABEL_CHARS, MAX_PLACEHOLDER_CHARS};

/// How far a `silence` reaches (design §2.3).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SilenceScope {
    /// This notification only, until it closes or expires.
    #[default]
    Notification,
    /// Everything sharing the notification's `group_key`. Always time-boxed —
    /// muting a whole group with no end date is the kind of thing someone does at
    /// 3am and regrets for a quarter.
    Group,
}

/// The discriminant of an [`Action`], used to report a mismatched interaction.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Opens a link, client-side only.
    View,
    /// Idempotent acknowledgement.
    Ack,
    /// Free-text answer.
    Reply,
    /// One exclusive option among several.
    Choice,
    /// Server-side snooze for the acting user.
    Silence,
}

impl ActionKind {
    /// The name used in error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Ack => "ack",
            Self::Reply => "reply",
            Self::Choice => "choice",
            Self::Silence => "silence",
        }
    }
}

/// One option of a `choice` action.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChoiceOption {
    /// The value recorded when this option is picked.
    pub key: ChoiceOptionKey,
    /// What the user reads.
    pub label: String,
}

/// A validated call to action.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// Open a link. Client-side only, produces no interaction.
    View {
        /// Button text.
        label: String,
        /// Where to go. `http(s)` only, checked here and again before opening.
        url: Url,
    },
    /// Acknowledge. Idempotent per user and per key: clicking twice is a no-op
    /// (design §2.3, scenario T4).
    Ack {
        /// Button text.
        label: String,
        /// Stable key the acknowledgement is recorded under.
        key: ActionKey,
    },
    /// Answer in free text.
    Reply {
        /// Button text.
        label: String,
        /// Hint shown in the empty input.
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    /// Pick exactly one option. Last pick wins, consistently with the rest of the
    /// model (design §2.2).
    Choice {
        /// Button text.
        label: String,
        /// Stable key the pick is recorded under.
        key: ActionKey,
        /// The options offered, in display order.
        options: Vec<ChoiceOption>,
    },
    /// Stop being notified about this, for the acting user only.
    Silence {
        /// Button text.
        label: String,
        /// How far the silence reaches.
        scope: SilenceScope,
        /// How long it lasts. Required for [`SilenceScope::Group`].
        #[serde(skip_serializing_if = "Option::is_none")]
        duration: Option<HumanDuration>,
    },
}

impl Action {
    /// Which kind this is.
    #[must_use]
    pub const fn kind(&self) -> ActionKind {
        match self {
            Self::View { .. } => ActionKind::View,
            Self::Ack { .. } => ActionKind::Ack,
            Self::Reply { .. } => ActionKind::Reply,
            Self::Choice { .. } => ActionKind::Choice,
            Self::Silence { .. } => ActionKind::Silence,
        }
    }

    /// The key interactions address this action by, when it has one.
    ///
    /// `view` and `silence` have none: neither records an interaction, so neither
    /// needs to be addressable after the fact.
    #[must_use]
    pub const fn key(&self) -> Option<&ActionKey> {
        match self {
            Self::Ack { key, .. } | Self::Choice { key, .. } => Some(key),
            Self::View { .. } | Self::Reply { .. } | Self::Silence { .. } => None,
        }
    }

    /// Button text.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::View { label, .. }
            | Self::Ack { label, .. }
            | Self::Reply { label, .. }
            | Self::Choice { label, .. }
            | Self::Silence { label, .. } => label,
        }
    }
}

/// What a publisher sends, before validation.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionDraft {
    /// See [`Action::View`].
    View {
        /// Button text.
        label: String,
        /// Target link, still a bare string at this point.
        url: String,
    },
    /// See [`Action::Ack`].
    Ack {
        /// Button text.
        label: String,
        /// Key to record under.
        key: String,
    },
    /// See [`Action::Reply`].
    Reply {
        /// Button text.
        label: String,
        /// Optional input hint.
        #[serde(default)]
        placeholder: Option<String>,
    },
    /// See [`Action::Choice`].
    Choice {
        /// Button text.
        label: String,
        /// Key to record under.
        key: String,
        /// The options offered.
        options: Vec<ChoiceOptionDraft>,
    },
    /// See [`Action::Silence`].
    Silence {
        /// Button text.
        label: String,
        /// Reach of the silence; defaults to this notification alone.
        #[serde(default)]
        scope: SilenceScope,
        /// How long it lasts.
        #[serde(default)]
        duration: Option<HumanDuration>,
    },
}

/// An unvalidated [`ChoiceOption`].
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceOptionDraft {
    /// Value to record.
    pub key: String,
    /// What the user reads.
    pub label: String,
}

impl TryFrom<ActionDraft> for Action {
    type Error = DomainError;

    fn try_from(draft: ActionDraft) -> Result<Self> {
        let label =
            |raw: &str| required_text(raw, TextKind::SingleLine, MAX_LABEL_CHARS, "action.label");

        Ok(match draft {
            ActionDraft::View { label: text, url } => Self::View {
                label: label(&text)?,
                url: parse_web_url(&url)?,
            },
            ActionDraft::Ack { label: text, key } => Self::Ack {
                label: label(&text)?,
                key: ActionKey::parse(&key)?,
            },
            ActionDraft::Reply {
                label: text,
                placeholder,
            } => Self::Reply {
                label: label(&text)?,
                placeholder: optional_text(
                    placeholder.as_deref(),
                    TextKind::SingleLine,
                    MAX_PLACEHOLDER_CHARS,
                    "action.placeholder",
                )?,
            },
            ActionDraft::Choice {
                label: text,
                key,
                options,
            } => Self::Choice {
                label: label(&text)?,
                key: ActionKey::parse(&key)?,
                options: validate_options(options)?,
            },
            ActionDraft::Silence {
                label: text,
                scope,
                duration,
            } => {
                if scope == SilenceScope::Group && duration.is_none() {
                    return Err(DomainError::SilenceGroupNeedsDuration);
                }
                Self::Silence {
                    label: label(&text)?,
                    scope,
                    duration,
                }
            }
        })
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        ActionDraft::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

/// Accept only links a client may open. Everything else — `javascript:`, `file:`,
/// a custom scheme that would reach `xdg-open` — is refused at ingestion so it
/// never reaches a client at all (design §2.3, scenario T4).
fn parse_web_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw.trim()).map_err(|_| DomainError::Malformed {
        field: "action.url",
        reason: "not a valid absolute URL",
    })?;

    match url.scheme() {
        "http" | "https" => Ok(url),
        scheme => Err(DomainError::UrlSchemeNotAllowed {
            scheme: scheme.to_owned(),
        }),
    }
}

fn validate_options(drafts: Vec<ChoiceOptionDraft>) -> Result<Vec<ChoiceOption>> {
    if drafts.is_empty() {
        return Err(DomainError::Empty {
            field: "action.options",
        });
    }
    if drafts.len() > MAX_CHOICE_OPTIONS {
        return Err(DomainError::TooMany {
            what: "action options",
            max: MAX_CHOICE_OPTIONS,
            actual: drafts.len(),
        });
    }

    let mut options = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let option = ChoiceOption {
            key: ChoiceOptionKey::parse(&draft.key)?,
            label: required_text(
                &draft.label,
                TextKind::SingleLine,
                MAX_LABEL_CHARS,
                "action.options.label",
            )?,
        };
        if options
            .iter()
            .any(|existing: &ChoiceOption| existing.key == option.key)
        {
            return Err(DomainError::DuplicateActionKey {
                key: option.key.to_string(),
            });
        }
        options.push(option);
    }
    Ok(options)
}

/// The action set of one revision: at most [`MAX_ACTIONS`], with unique keys.
///
/// Order is meaningful — it is display priority. Mobile push transports only
/// surface the first two or three, the rest are reachable by opening the client
/// (design §2.3).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Actions(Vec<Action>);

impl Actions {
    /// Validate a list of actions as a set.
    pub fn new(actions: Vec<Action>) -> Result<Self> {
        if actions.len() > MAX_ACTIONS {
            return Err(DomainError::TooMany {
                what: "actions",
                max: MAX_ACTIONS,
                actual: actions.len(),
            });
        }

        // Two actions sharing a key would make an interaction on that key
        // ambiguous — and `ack` idempotency is defined per key.
        let mut seen: Vec<&ActionKey> = Vec::with_capacity(actions.len());
        for key in actions.iter().filter_map(Action::key) {
            if seen.contains(&key) {
                return Err(DomainError::DuplicateActionKey {
                    key: key.to_string(),
                });
            }
            seen.push(key);
        }

        Ok(Self(actions))
    }

    /// The actions, in display order.
    #[must_use]
    pub fn as_slice(&self) -> &[Action] {
        &self.0
    }

    /// Look up the action an interaction is addressed to.
    #[must_use]
    pub fn find(&self, key: &ActionKey) -> Option<&Action> {
        self.0.iter().find(|action| action.key() == Some(key))
    }

    /// Whether there are no actions at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many actions there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl<'de> Deserialize<'de> for Actions {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        Self::new(Vec::<Action>::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    fn view(url: &str) -> Result<Action> {
        ActionDraft::View {
            label: "Runbook".to_owned(),
            url: url.to_owned(),
        }
        .try_into()
    }

    fn ack(key: &str) -> Action {
        ActionDraft::Ack {
            label: "Ack".to_owned(),
            key: key.to_owned(),
        }
        .try_into()
        .unwrap()
    }

    // --- T4 · Actions (design §3.4) ---------------------------------------

    #[test]
    fn critical_t4_view_accepts_only_web_urls() {
        assert!(view("https://runbook.weebo.poc/etcd").is_ok());
        assert!(view("http://runbook.weebo.poc/etcd").is_ok());

        for hostile in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "weebo://open?cmd=rm",
            "data:text/html,<script>",
        ] {
            let err = view(hostile).unwrap_err();
            assert_eq!(
                err.code(),
                "action.url_scheme_not_allowed",
                "`{hostile}` must never reach a client"
            );
        }
        // Not a scheme problem, a shape problem.
        assert_eq!(view("not a url").unwrap_err().code(), "field.malformed");
    }

    #[test]
    fn labels_are_sanitised_and_bounded() {
        let action: Action = ActionDraft::Ack {
            label: "\u{1b}[31mAck\u{1b}[0m".to_owned(),
            key: "ack-incident".to_owned(),
        }
        .try_into()
        .unwrap();
        assert_eq!(action.label(), "Ack");

        let too_long: Result<Action> = ActionDraft::Ack {
            label: "x".repeat(MAX_LABEL_CHARS + 1),
            key: "ack-incident".to_owned(),
        }
        .try_into();
        assert_eq!(too_long.unwrap_err().code(), "field.too_long");
    }

    #[test]
    fn a_group_silence_must_be_time_boxed() {
        let unbounded: Result<Action> = ActionDraft::Silence {
            label: "Silence".to_owned(),
            scope: SilenceScope::Group,
            duration: None,
        }
        .try_into();
        assert_eq!(
            unbounded.unwrap_err(),
            DomainError::SilenceGroupNeedsDuration
        );

        // Scoped to the notification, "until it closes" is a fine answer.
        let bounded: Result<Action> = ActionDraft::Silence {
            label: "Silence".to_owned(),
            scope: SilenceScope::Notification,
            duration: None,
        }
        .try_into();
        assert!(bounded.is_ok());
    }

    #[test]
    fn a_choice_needs_options_and_they_must_be_distinct() {
        let none: Result<Action> = ActionDraft::Choice {
            label: "Escalade".to_owned(),
            key: "escalate".to_owned(),
            options: vec![],
        }
        .try_into();
        assert_eq!(none.unwrap_err().code(), "field.empty");

        let duplicated: Result<Action> = ActionDraft::Choice {
            label: "Escalade".to_owned(),
            key: "escalate".to_owned(),
            options: vec![
                ChoiceOptionDraft {
                    key: "oncall".to_owned(),
                    label: "Page on-call".to_owned(),
                },
                ChoiceOptionDraft {
                    key: "oncall".to_owned(),
                    label: "Again".to_owned(),
                },
            ],
        }
        .try_into();
        assert_eq!(duplicated.unwrap_err().code(), "action.duplicate_key");
    }

    #[test]
    fn the_action_set_is_capped_and_keyed_uniquely() {
        let five = (0..MAX_ACTIONS)
            .map(|i| ack(&format!("ack-{i}")))
            .collect::<Vec<_>>();
        assert_eq!(Actions::new(five).unwrap().len(), MAX_ACTIONS);

        let six = (0..=MAX_ACTIONS)
            .map(|i| ack(&format!("ack-{i}")))
            .collect::<Vec<_>>();
        assert_eq!(Actions::new(six).unwrap_err().code(), "collection.too_many");

        let colliding = vec![ack("ack-incident"), ack("ack-incident")];
        assert_eq!(
            Actions::new(colliding).unwrap_err().code(),
            "action.duplicate_key"
        );
    }

    #[test]
    fn keyless_actions_never_collide() {
        // Two view buttons are perfectly ordinary; neither is addressable.
        let both = vec![
            view("https://a.weebo.poc").unwrap(),
            view("https://b.weebo.poc").unwrap(),
        ];
        assert!(Actions::new(both).is_ok());
    }

    #[test]
    fn actions_round_trip_through_json() {
        let actions = Actions::new(vec![
            view("https://runbook.weebo.poc").unwrap(),
            ack("ack-incident"),
        ])
        .unwrap();

        let json = serde_json::to_string(&actions).unwrap();
        assert_eq!(serde_json::from_str::<Actions>(&json).unwrap(), actions);
    }

    #[test]
    fn critical_t4_deserialising_re_runs_validation() {
        // Storage is not a trust boundary we get to skip: a row written by an older
        // build must still be refused if it violates today's invariants.
        let json = r#"[{"type":"view","label":"x","url":"javascript:alert(1)"}]"#;
        assert!(serde_json::from_str::<Actions>(json).is_err());
    }
}
