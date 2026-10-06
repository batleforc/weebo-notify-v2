//! What a notification says (§2.3): the publisher-controlled, attacker-influenced
//! half of a revision, constructible only through validation (storage included).

use serde::{Deserialize, Serialize};

use crate::action::Actions;
use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, optional_text, required_text, sanitize};
use crate::ids::AttachmentId;
use crate::limits::{
    MAX_BODY_CHARS, MAX_DATA_URI_BYTES, MAX_GROUP_KEY_CHARS, MAX_ICON_CHARS, MAX_PAYLOAD_BYTES,
    MAX_TAG_CHARS, MAX_TAGS, MAX_TITLE_CHARS,
};

/// How far along a long-running thing is, in the publisher's own units.
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

/// The image shown with a notification: an uploaded attachment, or a small inline `data:` URI (§2.3, §6.1).
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
    /// A small image inlined in the payload, capped at [`MAX_DATA_URI_BYTES`].
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

/// Inline image types the server accepts (no animated formats, §2.4).
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
    /// One line, always present; what a push shows when there is room for nothing else.
    pub title: String,
    /// Markdown; raw HTML is inert, escaped at render time (§2.3, §10.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Progress of a long-running operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    /// Illustration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<Image>,
    /// Name of a client-side icon, e.g. `cluster`; unknown names fall back to the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Buttons, in display priority order.
    #[serde(skip_serializing_if = "Actions::is_empty")]
    pub actions: Actions,
    /// Free-form labels, normalised to lowercase and deduplicated.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Client-side grouping key, and the unit a group-scoped silence acts on (§2.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_key: Option<String>,
}

/// What a publisher sends, before validation (unknown fields tolerated because it is flattened).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContentDraft {
    /// See [`Content::title`]; absent means "inherit" on a revision and is refused on creation.
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

    /// Fill the gaps from the superseded revision (§2.2), so a CI job can close with
    /// just `state`/`status`. **Actions are never inherited** (§2.3, T4); absent and
    /// `null` are the same, so clearing a body takes an empty string.
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
        let content = Self::validated(draft)?;

        // The whole-payload ceiling (§8.2, T15): per-field limits do not add up to it,
        // so measure the serialised form after validation.
        let size = serde_json::to_vec(&content).map_or(0, |bytes| bytes.len());
        if size > MAX_PAYLOAD_BYTES {
            return Err(DomainError::TooLarge {
                what: "payload",
                max: MAX_PAYLOAD_BYTES,
                actual: size,
            });
        }

        Ok(content)
    }
}

impl Content {
    /// Every field checked against its own limit, without the whole-payload one.
    fn validated(draft: ContentDraft) -> Result<Self> {
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
        // Close the pipeline without restating title, body or tags.
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
        // T4: a closed pipeline must not still offer "Cancel".
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
    fn critical_t15_fields_that_each_fit_can_still_make_a_payload_that_does_not() {
        // T15: every field is within its own limit (multi-byte body, ~48 KiB image),
        // yet together they exceed 64 KiB.
        let draft = ContentDraft {
            title: "an incident".to_owned(),
            body: Some("é".repeat(MAX_BODY_CHARS)),
            image: Some(
                ImageDraft::DataUri {
                    uri: format!(
                        "data:image/png;base64,{}",
                        "A".repeat(MAX_DATA_URI_BYTES - 32)
                    ),
                }
                .try_into()
                .unwrap(), // just under the inline image cap, so it validates
            ),
            ..ContentDraft::default()
        };

        let refused: Result<Content> = draft.try_into();

        assert_eq!(
            refused.unwrap_err().code(),
            "payload.too_large",
            "each field fits; the payload does not (§8.2)"
        );
    }

    #[test]
    fn critical_t15_a_payload_under_the_ceiling_is_accepted_whole() {
        // A body plus a small screenshot must still fit.
        let draft = ContentDraft {
            title: "etcd degraded".to_owned(),
            body: Some("il y a de la latence sur le cluster".repeat(100)),
            image: Some(
                ImageDraft::DataUri {
                    uri: format!("data:image/png;base64,{}", "A".repeat(8 * 1024)),
                }
                .try_into()
                .unwrap(), // a small screenshot
            ),
            ..ContentDraft::default()
        };

        let accepted: Result<Content> = draft.try_into();

        assert!(accepted.is_ok(), "{:?}", accepted.err());
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

    // --- T5 · Hygiene -----------------------------------------------------

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

        // Escapes are stripped along with their OSC argument.
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
        // HTML is escaped at render time (§2.3, §10.3), not stripped.
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
