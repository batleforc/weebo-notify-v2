//! Mapping a received JSON body to notification fields (RFC 0003): one operator-
//! declared transform per id, evaluated by an adapter so no interpreter type
//! crosses the port. The registry comes from config, swapped on `SIGHUP`.

use std::sync::Arc;

use notify_domain::{ChannelId, Priority, State, Status};

use crate::feeds::FeedLang;

/// Whether the received JSON is appended to the body (RFC 0003 §4.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RawMode {
    #[default]
    Off,
    /// The current element of `split`.
    Item,
    /// The whole body.
    Payload,
}

/// One element's fields, as the transform's programs produced them. Strings are
/// not yet cut or sanitised: that is the publish path's job.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mapped {
    pub title: Option<String>,
    pub body: Option<String>,
    pub link: Option<String>,
    pub external_id: Option<String>,
    pub group_key: Option<String>,
    pub ack: Option<String>,
    pub tags: Vec<String>,
    pub priority: Option<Priority>,
    pub status: Option<Status>,
    pub state: Option<State>,
    /// The element itself: the `split` item, or the whole body without `split`.
    pub element: serde_json::Value,
    /// The whole body, shared by every element of a request.
    pub payload: Arc<serde_json::Value>,
    /// The fields whose program failed, with a fixed kind (`no_such_key`, `type`,
    /// `overload`, `other`) and never the interpreter's text, which quotes the body.
    pub failed: Vec<(&'static str, &'static str)>,
}

/// Why a body gave no elements. Each maps to one `outcome` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransformError {
    #[error("the body is not JSON")]
    NotJson,
    #[error("split is not a list")]
    NotList,
    #[error("split yields {actual} elements, more than the {max} allowed")]
    TooMany { max: usize, actual: usize },
    /// Every evaluation slot is taken.
    #[error("every evaluation slot is busy")]
    Busy,
    /// The evaluation did not end within its deadline.
    #[error("the evaluation took too long")]
    Timeout,
    /// The interpreter panicked; caught, never the caller's fault.
    #[error("the evaluation failed")]
    Internal,
}

impl TransformError {
    /// The `outcome` label of `notify_transform_requests_total` (RFC 0003 §10.1).
    #[must_use]
    pub const fn outcome(self) -> &'static str {
        match self {
            Self::NotJson => "not_json",
            Self::NotList => "not_list",
            Self::TooMany { .. } => "too_many",
            Self::Busy => "busy",
            Self::Timeout => "timeout",
            Self::Internal => "internal",
        }
    }
}

/// One declared `[[transforms]]` entry, compiled.
#[async_trait::async_trait]
pub trait Transform: Send + Sync {
    fn id(&self) -> &str;
    /// The channels it may serve; nothing else.
    fn channels(&self) -> &[ChannelId];
    /// Revise an element already published, rather than skip it.
    fn update(&self) -> bool;
    fn raw(&self) -> RawMode;
    fn lang(&self) -> FeedLang;
    /// The title when the `title` program fails or is empty.
    fn title_fallback(&self) -> &str;
    /// Parse `body` and run every program over every element, all before the
    /// caller publishes anything.
    async fn evaluate(&self, body: Vec<u8>) -> Result<Vec<Mapped>, TransformError>;
}

/// An element's identity when its transform names none (RFC 0003 §4.2): the
/// [`crate::FEED_KEY_HEX`]-character digest of its JSON, keys sorted by `serde_json`'s
/// `BTreeMap`, so key order in the body does not matter.
#[must_use]
pub fn element_digest(element: &serde_json::Value) -> String {
    crate::feeds::digest(&[&element.to_string()])
}

/// What config declares. Synchronous, like [`crate::FeedRegistry`].
pub trait TransformRegistry: Send + Sync {
    fn get(&self, id: &str) -> Option<Arc<dyn Transform>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_ignores_key_order() {
        let a: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        assert_eq!(element_digest(&a), element_digest(&b));
        assert_eq!(element_digest(&a).len(), crate::FEED_KEY_HEX);
    }
}
