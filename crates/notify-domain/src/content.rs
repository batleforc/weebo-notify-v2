//! What a notification actually says (design §2.3).
//!
//! Content is the publisher-controlled half of a revision: title, markdown body,
//! progress, image, actions, tags. Everything here is attacker-influenced text, so
//! the type is only constructible through validation — there is no way to build a
//! [`Content`] that has not been through [`hygiene`](crate::hygiene), including
//! when it comes back out of storage.
//!
//! The lifecycle half — `state`, `status`, priority, TTL — lives on the revision
//! rather than in the content, because that is what the state machine reads and
//! what a client filters an inbox on.

use serde::{Deserialize, Serialize};

use crate::action::Actions;
use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, optional_text, required_text, sanitize};
use crate::ids::AttachmentId;
use crate::limits::{
    MAX_BODY_CHARS, MAX_DATA_URI_BYTES, MAX_GROUP_KEY_CHARS, MAX_ICON_CHARS, MAX_TAG_CHARS,
    MAX_TAGS, MAX_TITLE_CHARS,
};

/// How far along a long-running thing is. Rendered as a bar; the units are the
/// publisher's own (steps, files, percent).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ProgressDraft")]
pub struct Progress {
    /// Where it is now.
    pub current: u32,
    /// Where it finishes.
    pub total: u32,
}

/// An unvalidated [`Progress`].
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressDraft {
    /// Where it is now.
    pub current: u32,
    /// Where it finishes.
    pub total: u32,
}

impl TryFrom<ProgressDraft> for Progress {
    type Error = DomainError;

    fn try_from(draft: ProgressDraft) -> Result<Self> {
        if draft.total == 0 {
            return Err(DomainError::Malformed {
                field: "payload.progress.total",
                reason: "must be at least 1",
            });
        }
        if draft.current > draft.total {
            return Err(DomainError::Malformed {
                field: "payload.progress.current",
                reason: "must not exceed `total`",
            });
        }
        Ok(Self {
            current: draft.current,
            total: draft.total,
        })
    }
}

/// The image shown with a notification.
///
/// Anything of real size goes to the object store and is referenced, so the
/// payload stays small enough to ride inside a push transport (design §2.3, §6.1).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Image {
    /// A file already uploaded to the bucket through a presigned URL.
    Attachment {
        /// The uploaded object.
        #[serde(rename = "ref")]
        reference: AttachmentId,
    },
    /// A small image inlined in the payload. Capped at [`MAX_DATA_URI_BYTES`];
    /// past that, publishers upload an attachment instead.
    DataUri {
        /// The `data:` URI, already checked for type and size.
        uri: String,
    },
}

/// An unvalidated [`Image`].
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageDraft {
    /// See [`Image::Attachment`].
    Attachment {
        /// The uploaded object.
        #[serde(rename = "ref")]
        reference: AttachmentId,
    },
    /// See [`Image::DataUri`].
    DataUri {
        /// The `data:` URI.
        uri: String,
    },
}

/// Inline image types the server accepts. Animated formats are excluded for the
/// same reason as on avatars (design §2.4): they are the expensive end of image
/// decoding and nothing here needs them.
const ALLOWED_DATA_URI_PREFIXES: [&str; 3] = [
    "data:image/png;base64,",
    "data:image/jpeg;base64,",
    "data:image/webp;base64,",
];

impl TryFrom<ImageDraft> for Image {
    type Error = DomainError;

    fn try_from(draft: ImageDraft) -> Result<Self> {
        Ok(match draft {
            ImageDraft::Attachment { reference } => Self::Attachment { reference },
            ImageDraft::DataUri { uri } => {
                let uri = uri.trim().to_owned();
                if !ALLOWED_DATA_URI_PREFIXES
                    .iter()
                    .any(|prefix| uri.starts_with(prefix))
                {
                    return Err(DomainError::Malformed {
                        field: "payload.image.uri",
                        reason: "expected a base64 `data:` URI of type png, jpeg or webp",
                    });
                }
                if uri.len() > MAX_DATA_URI_BYTES {
                    return Err(DomainError::TooLarge {
                        what: "inline image",
                        max: MAX_DATA_URI_BYTES,
                        actual: uri.len(),
                    });
                }
                Self::DataUri { uri }
            }
        })
    }
}

impl<'de> Deserialize<'de> for Image {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        ImageDraft::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

/// The validated body of a revision.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Content {
    /// One line, always present. What a push notification shows when there is room
    /// for nothing else.
    pub title: String,
    /// Markdown. Raw HTML in it is inert by contract, escaped at render time under
    /// a strict CSP (design §2.3, §10.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Progress of a long-running operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    /// Illustration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<Image>,
    /// Name of a client-side icon, e.g. `cluster`. Clients that do not know it
    /// fall back to their default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Buttons, in display priority order.
    #[serde(skip_serializing_if = "Actions::is_empty")]
    pub actions: Actions,
    /// Free-form labels, normalised to lowercase and deduplicated.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Client-side grouping key, and the unit a group-scoped silence acts on
    /// (design §2.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_key: Option<String>,
}

/// What a publisher sends, before validation.
///
/// Unknown fields are tolerated here, unlike in the nested drafts: this struct is
/// flattened into the publish request alongside the lifecycle fields (`state`,
/// `status`, `ttl`…), and serde cannot reject unknown fields across a flatten.
/// The nested drafts keep the check, which is where it earns its keep — a
/// misspelled `url` in a `view` action would otherwise silently produce a button
/// that goes nowhere.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContentDraft {
    /// See [`Content::title`].
    ///
    /// Optional on the wire, and absent means "leave it alone" on a revision — see
    /// [`ContentDraft::inheriting`]. On a creation there is nothing to inherit, so
    /// an absent title is the same refusal an empty one gets.
    #[serde(default)]
    pub title: String,
    /// See [`Content::body`].
    #[serde(default)]
    pub body: Option<String>,
    /// See [`Content::progress`].
    #[serde(default)]
    pub progress: Option<Progress>,
    /// See [`Content::image`].
    #[serde(default)]
    pub image: Option<Image>,
    /// See [`Content::icon`].
    #[serde(default)]
    pub icon: Option<String>,
    /// See [`Content::actions`].
    #[serde(default)]
    pub actions: Actions,
    /// See [`Content::tags`].
    #[serde(default)]
    pub tags: Vec<String>,
    /// See [`Content::group_key`].
    #[serde(default)]
    pub group_key: Option<String>,
}

impl ContentDraft {
    /// A draft carrying nothing but a title — the shape of most publishes.
    #[must_use]
    pub fn titled(title: &str) -> Self {
        Self {
            title: title.to_owned(),
            ..Self::default()
        }
    }

    /// Fill the gaps from the revision this one supersedes (design §2.2).
    ///
    /// This is what lets a CI job close a pipeline with
    /// `{"state": "closed", "status": "success"}` and keep its title, its tags and
    /// its body. Without it every revision would have to restate the whole payload,
    /// and the second half of every publish/close pair would be a copy-paste of the
    /// first — which is where they drift.
    ///
    /// **Actions are never inherited.** They are replaced wholesale on every
    /// revision (design §2.3, scenario T4): a closed pipeline should not still be
    /// offering "Cancel", and the only way to be sure of that is for silence to
    /// mean "no buttons" rather than "the previous buttons".
    ///
    /// The limitation worth knowing: absent and explicitly `null` are the same
    /// thing here, so this revision shape cannot *clear* a body. Publishing an
    /// empty string does.
    #[must_use]
    pub fn inheriting(mut self, head: &Content) -> Self {
        if self.title.trim().is_empty() {
            self.title.clone_from(&head.title);
        }
        if self.body.is_none() {
            self.body.clone_from(&head.body);
        }
        if self.progress.is_none() {
            self.progress = head.progress;
        }
        if self.image.is_none() {
            self.image.clone_from(&head.image);
        }
        if self.icon.is_none() {
            self.icon.clone_from(&head.icon);
        }
        if self.tags.is_empty() {
            self.tags.clone_from(&head.tags);
        }
        if self.group_key.is_none() {
            self.group_key.clone_from(&head.group_key);
        }
        self
    }
}

impl TryFrom<ContentDraft> for Content {
    type Error = DomainError;

    fn try_from(draft: ContentDraft) -> Result<Self> {
        Ok(Self {
            title: required_text(
                &draft.title,
                TextKind::SingleLine,
                MAX_TITLE_CHARS,
                "payload.title",
            )?,
            body: optional_text(
                draft.body.as_deref(),
                TextKind::MultiLine,
                MAX_BODY_CHARS,
                "payload.body",
            )?,
            progress: draft.progress,
            image: draft.image,
            icon: optional_text(
                draft.icon.as_deref(),
                TextKind::SingleLine,
                MAX_ICON_CHARS,
                "payload.icon",
            )?,
            actions: draft.actions,
            tags: validate_tags(&draft.tags)?,
            group_key: optional_text(
                draft.group_key.as_deref(),
                TextKind::SingleLine,
                MAX_GROUP_KEY_CHARS,
                "payload.group_key",
            )?,
        })
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        ContentDraft::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

/// Sanitise, lowercase, drop empties, deduplicate, then check the count.
///
/// Lowercasing is what makes `?tag=prod` a question with one answer: normalising
/// once at ingestion beats hoping every filter, index and client agrees on
/// case-insensitive comparison.
fn validate_tags(raw: &[String]) -> Result<Vec<String>> {
    let mut tags: Vec<String> = Vec::with_capacity(raw.len());

    for tag in raw {
        let tag = sanitize(tag, TextKind::SingleLine).to_lowercase();
        if tag.is_empty() {
            continue;
        }
        let len = tag.chars().count();
        if len > MAX_TAG_CHARS {
            return Err(DomainError::TooLong {
                field: "payload.tags",
                max: MAX_TAG_CHARS,
                actual: len,
            });
        }
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }

    if tags.len() > MAX_TAGS {
        return Err(DomainError::TooMany {
            what: "tags",
            max: MAX_TAGS,
            actual: tags.len(),
        });
    }
    Ok(tags)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use crate::action::Action;

    use super::*;

    fn content(draft: ContentDraft) -> Result<Content> {
        draft.try_into()
    }

    #[test]
    fn a_title_is_required_and_sanitised() {
        let ok = content(ContentDraft::titled(
            "\u{1b}[2Jcluster-ovh-1 · etcd degraded",
        ))
        .unwrap();
        assert_eq!(ok.title, "cluster-ovh-1 · etcd degraded");

        assert_eq!(
            content(ContentDraft::titled("   ")).unwrap_err(),
            DomainError::Empty {
                field: "payload.title"
            }
        );
    }

    #[test]
    fn a_revision_that_says_nothing_keeps_what_the_head_said() {
        // The second half of the design's CI example: close the pipeline without
        // restating its title, its body or its tags.
        let head = content(ContentDraft {
            body: Some("build log attached".to_owned()),
            icon: Some("rocket".to_owned()),
            tags: vec!["ci".to_owned(), "infra".to_owned()],
            group_key: Some("run-4521".to_owned()),
            ..ContentDraft::titled("Pipeline infra #4521")
        })
        .unwrap();

        let revised = content(ContentDraft::default().inheriting(&head)).unwrap();

        assert_eq!(revised.title, "Pipeline infra #4521");
        assert_eq!(revised.body.as_deref(), Some("build log attached"));
        assert_eq!(revised.icon.as_deref(), Some("rocket"));
        assert_eq!(revised.tags, ["ci", "infra"]);
        assert_eq!(revised.group_key.as_deref(), Some("run-4521"));
    }

    #[test]
    fn what_a_revision_does_say_wins_over_the_head() {
        let head = content(ContentDraft {
            body: Some("running".to_owned()),
            ..ContentDraft::titled("Pipeline infra #4521")
        })
        .unwrap();

        let revised = content(
            ContentDraft {
                body: Some("failed on step 4".to_owned()),
                ..ContentDraft::titled("Pipeline infra #4521 — failed")
            }
            .inheriting(&head),
        )
        .unwrap();

        assert_eq!(revised.title, "Pipeline infra #4521 — failed");
        assert_eq!(revised.body.as_deref(), Some("failed on step 4"));
    }

    #[test]
    fn actions_are_replaced_wholesale_and_never_inherited() {
        // Scenario T4. A closed pipeline must not still be offering "Cancel", and
        // the only way to be sure is for silence to mean "no buttons".
        let head = content(ContentDraft {
            actions: Actions::new(vec![Action::View {
                label: "Logs".to_owned(),
                url: "https://forge.example/run/4521".parse().unwrap(),
            }])
            .unwrap(),
            ..ContentDraft::titled("Pipeline infra #4521")
        })
        .unwrap();
        assert_eq!(head.actions.len(), 1);

        let revised = content(ContentDraft::default().inheriting(&head)).unwrap();

        assert!(revised.actions.is_empty());
    }

    #[test]
    fn tags_are_normalised_deduplicated_and_capped() {
        let value = content(ContentDraft {
            tags: vec![
                "Etcd".to_owned(),
                "etcd".to_owned(),
                "  PROD ".to_owned(),
                String::new(),
            ],
            ..ContentDraft::titled("t")
        })
        .unwrap();
        assert_eq!(value.tags, ["etcd", "prod"]);

        let too_many = content(ContentDraft {
            tags: (0..=MAX_TAGS).map(|i| format!("tag{i}")).collect(),
            ..ContentDraft::titled("t")
        });
        assert_eq!(too_many.unwrap_err().code(), "collection.too_many");
    }

    #[test]
    fn inline_images_are_type_checked_and_size_capped() {
        let ok: Result<Image> = ImageDraft::DataUri {
            uri: "data:image/png;base64,AAAA".to_owned(),
        }
        .try_into();
        assert!(ok.is_ok());

        let wrong_type: Result<Image> = ImageDraft::DataUri {
            uri: "data:text/html;base64,AAAA".to_owned(),
        }
        .try_into();
        assert_eq!(wrong_type.unwrap_err().code(), "field.malformed");

        let too_big: Result<Image> = ImageDraft::DataUri {
            uri: format!("data:image/png;base64,{}", "A".repeat(MAX_DATA_URI_BYTES)),
        }
        .try_into();
        assert_eq!(too_big.unwrap_err().code(), "payload.too_large");
    }

    #[test]
    fn progress_must_be_coherent() {
        let backwards: Result<Progress> = ProgressDraft {
            current: 11,
            total: 10,
        }
        .try_into();
        assert!(backwards.is_err());

        let empty: Result<Progress> = ProgressDraft {
            current: 0,
            total: 0,
        }
        .try_into();
        assert!(empty.is_err());
    }

    // --- T5 · Hygiene (design §3.4) ---------------------------------------

    #[test]
    fn critical_t5_every_text_field_is_stripped_of_escapes_and_control_characters() {
        use crate::action::{ActionDraft, Actions};

        let hostile = "\u{1b}[2Jwiped\u{1b}]0;retitled\u{7}\u{0}\u{202e}";

        let value = content(ContentDraft {
            title: hostile.to_owned(),
            body: Some(format!("{hostile}\nsecond line")),
            icon: Some(hostile.to_owned()),
            tags: vec![hostile.to_owned()],
            group_key: Some(hostile.to_owned()),
            actions: Actions::new(vec![
                ActionDraft::Ack {
                    label: hostile.to_owned(),
                    key: "ack-incident".to_owned(),
                }
                .try_into()
                .unwrap(),
            ])
            .unwrap(),
            ..ContentDraft::default()
        })
        .unwrap();

        // What lands in a terminal, a toast and a browser is inert text.
        // The OSC argument goes with its introducer: `retitled` was the payload of
        // a window-title escape, not text the publisher wrote.
        assert_eq!(value.title, "wiped");
        assert_eq!(value.body.as_deref(), Some("wiped\nsecond line"));
        assert_eq!(value.icon.as_deref(), Some("wiped"));
        assert_eq!(value.tags, ["wiped"]);
        assert_eq!(value.group_key.as_deref(), Some("wiped"));
        assert_eq!(value.actions.as_slice()[0].label(), "wiped");

        let rendered = serde_json::to_string(&value).unwrap();
        assert!(
            !rendered.contains("\\u001b") && !rendered.contains('\u{1b}'),
            "no escape sequence may survive as far as the wire"
        );
    }

    #[test]
    fn critical_t5_raw_html_stays_inert_text_rather_than_being_mangled() {
        // The contract is markdown-only rendering under a strict CSP (design §2.3,
        // §10.3), not stripping at ingestion — which would eat `3 < 4`.
        let value = content(ContentDraft {
            body: Some("<img src=x onerror=alert(1)> and 3 < 4".to_owned()),
            ..ContentDraft::titled("t")
        })
        .unwrap();
        assert_eq!(
            value.body.as_deref(),
            Some("<img src=x onerror=alert(1)> and 3 < 4")
        );
    }

    #[test]
    fn content_round_trips_through_json() {
        let value = content(ContentDraft {
            body: Some("# incident\n\n- etcd".to_owned()),
            progress: Some(Progress {
                current: 3,
                total: 10,
            }),
            icon: Some("cluster".to_owned()),
            tags: vec!["etcd".to_owned()],
            group_key: Some("incident-4521".to_owned()),
            ..ContentDraft::titled("etcd degraded")
        })
        .unwrap();

        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<Content>(&json).unwrap(), value);
    }
}
