//! JSON webhooks mapped by declared transforms (RFC 0003): a producer POSTs its own
//! JSON, an operator-declared transform turns it into one notification per
//! element, and each goes through [`UpsertNotification`] as the producer.
//!
//! The order is the security argument (§4.2): `publish` on the channel first, so
//! nothing is looked up or evaluated for a caller who could not publish there;
//! then the transform and its allow-list, one answer for both; then every
//! program over every element, before the first write.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use notify_domain::hygiene::{TextKind, sanitize};
use notify_domain::limits::{
    MAX_BODY_CHARS, MAX_GROUP_KEY_CHARS, MAX_LABEL_CHARS, MAX_TAG_CHARS, MAX_TAGS, MAX_TITLE_CHARS,
};
use notify_domain::{
    Action, ActionDraft, Actions, ChannelId, ContentDraft, DomainError, ExternalId, RevisionDraft,
    Scope,
};
use notify_ports::{
    Failure, FeedLang, Mapped, Metrics, PortError, RawMode, Transform, TransformError,
    TransformRegistry, element_digest,
};

use crate::authz::{Authorizer, Caller};
use crate::feeds::open_label;
use crate::publish::{UpsertNotification, UpsertRequest, Upserted};

/// The metric label of a transform nobody declared, so a probe creates no series.
const UNKNOWN: &str = "_unknown";

/// Why a request published nothing more.
#[derive(Debug)]
pub enum IngestError {
    /// `403`, `422`, `429`, or the world failing.
    Refused(Failure),
    /// No such transform, or it does not list the channel: one answer for both.
    NotFound,
    /// No evaluation slot, or past the deadline: `503`, retry shortly.
    Busy,
}

impl From<Failure> for IngestError {
    fn from(failure: Failure) -> Self {
        Self::Refused(failure)
    }
}

/// `POST /compat/json/{transform}/{channel}`.
#[derive(Clone)]
pub struct IngestJson {
    authorizer: Authorizer,
    upsert: UpsertNotification,
    registry: Arc<dyn TransformRegistry>,
    metrics: Arc<dyn Metrics>,
    /// (transform, field) pairs already logged at `warn`.
    // shortcut: remembered per process, not per load; reset it on SIGHUP if operators miss a warn.
    warned: Arc<Mutex<HashSet<(String, &'static str)>>>,
}

impl core::fmt::Debug for IngestJson {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IngestJson").finish_non_exhaustive()
    }
}

impl IngestJson {
    #[must_use]
    pub fn new(
        authorizer: Authorizer,
        upsert: UpsertNotification,
        registry: Arc<dyn TransformRegistry>,
        metrics: Arc<dyn Metrics>,
    ) -> Self {
        Self {
            authorizer,
            upsert,
            registry,
            metrics,
            warned: Arc::default(),
        }
    }

    /// Whether `external_id` carries a declared transform's prefix, which only
    /// that transform may write (decision 15).
    #[must_use]
    pub fn reserves(&self, external_id: &str) -> bool {
        // An id is `[a-z0-9-]`, never `:`, so the prefix is everything before the first one.
        external_id
            .split_once(':')
            .is_some_and(|(prefix, _)| self.registry.get(prefix).is_some())
    }

    /// Refuse a native upsert under a reserved prefix (decision 15), only once the
    /// caller is known to hold `publish` there: otherwise its `422` would tell any key
    /// which transform ids exist, which `/compat/json`'s single `404` hides (T9).
    pub async fn refuse_reserved(
        &self,
        caller: &Caller,
        channel: &ChannelId,
        external_id: &str,
    ) -> Result<(), Failure> {
        if !self.reserves(external_id) {
            return Ok(());
        }
        self.authorizer
            .authorize(caller, channel, Scope::Publish)
            .await?;
        Err(DomainError::Malformed {
            field: "external_id",
            reason: "starts with a JSON webhook transform's reserved prefix",
        }
        .into())
    }

    /// Count a request the transport refused before reaching here (`too_large`, `form`).
    pub fn refused_early(&self, transform: &str, outcome: &'static str) {
        self.metrics
            .transform_request(self.label(transform), outcome);
    }

    fn label<'a>(&self, transform: &'a str) -> &'a str {
        if self.registry.get(transform).is_some() {
            transform
        } else {
            UNKNOWN
        }
    }

    /// Map `body` and publish each element, in order. Elements written before a
    /// `429` stay; the producer's retry skips them at no quota cost.
    #[tracing::instrument(name = "ingest_json", skip_all, fields(transform = %id, channel = %channel))]
    pub async fn execute(
        &self,
        caller: &Caller,
        id: &str,
        channel: ChannelId,
        body: Vec<u8>,
    ) -> Result<Vec<Upserted>, IngestError> {
        // Authorised once for the whole request: every element writes to this channel.
        let authorised = match self
            .authorizer
            .authorize(caller, &channel, Scope::Publish)
            .await
        {
            Ok(authorised) => authorised,
            Err(failure) => {
                self.metrics.transform_request(self.label(id), "forbidden");
                return Err(failure.into());
            }
        };
        let Some(transform) = self
            .registry
            .get(id)
            .filter(|transform| transform.channels().contains(&channel))
        else {
            let label = self.label(id);
            if label != UNKNOWN {
                tracing::warn!("a transform was called for a channel it does not list");
            }
            self.metrics.transform_request(label, "unlisted");
            return Err(IngestError::NotFound);
        };

        let started = std::time::Instant::now();
        let evaluated = transform.evaluate(body).await;
        self.metrics
            .transform_eval_seconds(id, started.elapsed().as_secs_f64());
        let elements = match evaluated {
            Ok(elements) => elements,
            Err(error) => {
                self.metrics.transform_request(id, error.outcome());
                return Err(refusal(error));
            }
        };

        let mut done = Vec::with_capacity(elements.len());
        let mut first_invalid = None;
        for (index, mapped) in elements.iter().enumerate() {
            self.count_failures(transform.as_ref(), mapped);
            let (external_id, digested) =
                external_id(transform.id(), mapped).map_err(Failure::Domain)?;
            let (draft, truncated) = transform_draft(mapped, transform.as_ref());
            if digested {
                self.metrics.transform_items(id, "id_digest", 1);
            }
            if truncated {
                self.metrics.transform_items(id, "raw_truncated", 1);
            }
            let request = UpsertRequest {
                channel: channel.clone(),
                external_id,
                draft,
            };
            let upserted = self
                .upsert
                .upsert_in(caller, &authorised, request, !transform.update())
                .await;
            match upserted {
                Ok(upserted) => {
                    let outcome = match upserted {
                        Upserted::Written(_) => "published",
                        Upserted::Skipped(_) => "skipped",
                    };
                    self.metrics.transform_items(id, outcome, 1);
                    done.push(upserted);
                }
                Err(Failure::Domain(error)) => {
                    tracing::warn!(index, field = error.code(), "an element was refused");
                    self.metrics.transform_items(id, "invalid", 1);
                    first_invalid.get_or_insert(error);
                }
                Err(failure) => {
                    let outcome = if matches!(failure, Failure::RateLimited { .. }) {
                        "refused"
                    } else {
                        "error"
                    };
                    self.metrics.transform_request(id, outcome);
                    return Err(failure.into());
                }
            }
        }
        match first_invalid {
            Some(error) if done.is_empty() => {
                self.metrics.transform_request(id, "all_invalid");
                Err(Failure::Domain(error).into())
            }
            _ => {
                self.metrics.transform_request(id, "ok");
                Ok(done)
            }
        }
    }

    /// Each failed field counts; the first per (transform, field) is a `warn`,
    /// the rest `debug`. The kind only: CEL's text quotes the body (T10).
    fn count_failures(&self, transform: &dyn Transform, mapped: &Mapped) {
        for &(field, kind) in &mapped.failed {
            self.metrics
                .transform_items(transform.id(), "eval_error", 1);
            let first = self
                .warned
                .lock()
                .is_ok_and(|mut warned| warned.insert((transform.id().to_owned(), field)));
            if first {
                tracing::warn!(field, kind, "a transform field failed; it is left unset");
            } else {
                tracing::debug!(field, kind, "a transform field failed; it is left unset");
            }
        }
    }
}

fn refusal(error: TransformError) -> IngestError {
    let malformed = |reason| {
        IngestError::Refused(Failure::Domain(DomainError::Malformed {
            field: "body",
            reason,
        }))
    };
    match error {
        TransformError::NotJson => malformed("not JSON"),
        TransformError::NotList => malformed("split is not a list"),
        TransformError::TooMany { max, actual } => {
            IngestError::Refused(Failure::Domain(DomainError::TooMany {
                what: "elements",
                max,
                actual,
            }))
        }
        TransformError::Busy | TransformError::Timeout => IngestError::Busy,
        TransformError::Internal => IngestError::Refused(Failure::Port(PortError::corrupt(
            "transform",
            "the interpreter panicked",
        ))),
    }
}

/// `<transform>:<result>`, or `<transform>:<digest of the element>` when the
/// program gave none or one `ExternalId` refuses. `true` when digested.
pub(crate) fn external_id(
    transform: &str,
    mapped: &Mapped,
) -> Result<(ExternalId, bool), DomainError> {
    if let Some(id) = mapped
        .external_id
        .as_deref()
        .and_then(|id| ExternalId::parse(&format!("{transform}:{id}")).ok())
    {
        return Ok((id, false));
    }
    // A transform id is ≤ 64 of [a-z0-9-] and the digest 32 hex: always valid.
    ExternalId::parse(&format!("{transform}:{}", element_digest(&mapped.element)))
        .map(|id| (id, true))
}

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The revision one element becomes: fallbacks, ceilings, the link and ack
/// actions, and the raw block. `true` when the raw block was truncated.
pub(crate) fn transform_draft(mapped: &Mapped, transform: &dyn Transform) -> (RevisionDraft, bool) {
    let title = [mapped.title.as_deref(), Some(transform.title_fallback())]
        .into_iter()
        .flatten()
        .map(|title| sanitize(&cut(title, MAX_TITLE_CHARS), TextKind::SingleLine))
        .find(|title| !title.is_empty())
        .unwrap_or_else(|| transform.id().to_owned());
    let body = mapped
        .body
        .as_deref()
        .filter(|body| !body.is_empty())
        .map(|body| cut(body, MAX_BODY_CHARS));
    let raw = match transform.raw() {
        RawMode::Off => None,
        RawMode::Item => Some(&mapped.element),
        RawMode::Payload => Some(mapped.payload.as_ref()),
    };
    let (body, truncated) = match raw {
        Some(json) => {
            let (body, truncated) = with_raw(body.as_deref(), json, transform.lang());
            (Some(body), truncated)
        }
        None => (body, false),
    };

    let link = mapped.link.as_ref().and_then(|url| {
        Action::try_from(ActionDraft::View {
            label: open_label(transform.lang()).to_owned(),
            url: url.clone(),
        })
        .ok()
    });
    let ack = mapped.ack.as_deref().and_then(|label| {
        Action::try_from(ActionDraft::Ack {
            label: cut(label, MAX_LABEL_CHARS),
            key: "ack".to_owned(),
        })
        .ok()
    });
    let actions = Actions::new(link.into_iter().chain(ack).collect()).unwrap_or_default();
    let tags = mapped
        .tags
        .iter()
        .filter(|tag| !tag.is_empty() && tag.chars().count() <= MAX_TAG_CHARS)
        .take(MAX_TAGS)
        .cloned()
        .collect();

    let mut draft = RevisionDraft::simple(ContentDraft {
        title,
        body,
        tags,
        actions,
        group_key: mapped
            .group_key
            .as_deref()
            .map(|key| cut(key, MAX_GROUP_KEY_CHARS)),
        ..ContentDraft::default()
    });
    draft.priority = mapped.priority;
    draft.status = mapped.status;
    draft.state = mapped.state;
    (draft, truncated)
}

/// `body`, a blank line, and `json` pretty-printed in a fenced block one backtick
/// longer than its longest run, cut to [`MAX_BODY_CHARS`] with the block closed
/// and a marker after it. `true` when cut.
pub(crate) fn with_raw(
    body: Option<&str>,
    json: &serde_json::Value,
    lang: FeedLang,
) -> (String, bool) {
    let pretty = serde_json::to_string_pretty(json).unwrap_or_default();
    let fence = "`".repeat(longest_backtick_run(&pretty).saturating_add(1).max(3));
    let head = body.map(|body| format!("{body}\n\n")).unwrap_or_default();
    let open = format!("{fence}json\n");
    let close = format!("\n{fence}");
    let whole = format!("{head}{open}{pretty}{close}");
    if whole.chars().count() <= MAX_BODY_CHARS {
        return (whole, false);
    }
    let marker = match lang {
        FeedLang::En => "\n\n_(truncated)_",
        FeedLang::Fr => "\n\n_(tronqué)_",
    };
    let frame = [open.as_str(), close.as_str(), marker]
        .iter()
        .fold(0_usize, |sum, part| {
            sum.saturating_add(part.chars().count())
        });
    // A fence as long as the limit leaves no block at all: the body and the marker.
    let Some(budget) = MAX_BODY_CHARS.checked_sub(frame) else {
        let room = MAX_BODY_CHARS.saturating_sub(marker.chars().count());
        return (
            format!("{}{marker}", cut(body.unwrap_or_default(), room)),
            true,
        );
    };
    // The body is cut too when it alone fills the budget, so the whole stays in bounds.
    let head = body
        .map(|body| format!("{}\n\n", cut(body, budget.saturating_sub(2))))
        .unwrap_or_default();
    let room = budget.saturating_sub(head.chars().count());
    (
        format!("{head}{open}{}{close}{marker}", cut(&pretty, room)),
        true,
    )
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

#[cfg(test)]
mod tests;
