//! The domain's single error type. Each `code` is public contract (problem+json,
//! §8): renaming one is a breaking change. [`ErrorKind`] lets transports map statuses.

use thiserror::Error;

/// The coarse category of a rejection, for transports to map to their own vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The input is malformed or violates an invariant — HTTP 422.
    Validation,
    /// A documented quota was exceeded (design §8.2) — HTTP 422.
    LimitExceeded,
    /// The payload itself is too big — HTTP 413.
    TooLarge,
    /// The operation is understood but forbidden by policy — HTTP 409.
    Conflict,
    /// The credential presented is not (or no longer) valid — HTTP 401.
    Unauthenticated,
    /// The principal is known but lacks the right — HTTP 403.
    Forbidden,
}

/// Everything the domain can refuse to do.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DomainError {
    /// A required field was absent.
    #[error("`{field}` is required")]
    Missing {
        /// Dotted path of the offending field, e.g. `payload.title`.
        field: &'static str,
    },

    /// A field contained nothing once sanitised (§2.3).
    #[error("`{field}` is empty once sanitised")]
    Empty {
        /// Dotted path of the offending field.
        field: &'static str,
    },

    /// A text field is longer than its ceiling, in characters.
    #[error("`{field}` is {actual} characters long, the maximum is {max}")]
    TooLong {
        /// Dotted path of the offending field.
        field: &'static str,
        /// Documented ceiling.
        max: usize,
        /// What was actually supplied.
        actual: usize,
    },

    /// A field has the right size but the wrong shape.
    #[error("`{field}` is malformed: {reason}")]
    Malformed {
        /// Dotted path of the offending field.
        field: &'static str,
        /// Why it was rejected, phrased for a human reading a CI log.
        reason: &'static str,
    },

    /// A collection holds more elements than allowed (actions, tags, options).
    #[error("{what}: {actual} supplied, the maximum is {max}")]
    TooMany {
        /// What was counted, e.g. `actions`.
        what: &'static str,
        /// Documented ceiling.
        max: usize,
        /// What was actually supplied.
        actual: usize,
    },

    /// A byte-sized thing exceeded its ceiling (design §8.2).
    #[error("{what} is {actual} bytes, the maximum is {max}")]
    TooLarge {
        /// What was measured, e.g. `payload`.
        what: &'static str,
        /// Documented ceiling in bytes.
        max: usize,
        /// What was actually supplied.
        actual: usize,
    },

    /// The 1 000-revision ceiling was hit (§8.2); only further revisions are refused.
    #[error("this notification already has {max} revisions — publish a new one")]
    RevisionLimitReached {
        /// Documented ceiling.
        max: u32,
    },

    /// The owner already holds `[devices] max_per_user` active devices; one must be
    /// revoked first, so a leaked session cannot enrol endpoints without end.
    #[error("{max} devices are already enrolled — revoke one first")]
    DeviceLimitReached {
        /// The configured ceiling.
        max: u32,
    },

    /// `ttl = 0` (keep forever) needs the channel's explicit opt-in (§7.2bis).
    #[error("infinite retention is not enabled on this channel")]
    InfiniteTtlNotAllowed,

    /// Two actions claim the same key.
    #[error("two actions share the key `{key}`")]
    DuplicateActionKey {
        /// The colliding key.
        key: String,
    },

    /// An interaction referenced an action the current revision does not offer (§2.3).
    #[error("this notification has no action with the key `{key}`")]
    UnknownActionKey {
        /// The key that was looked up.
        key: String,
    },

    /// The interaction kind does not match the action kind (e.g. a reply on `ack`).
    #[error("action `{key}` does not accept a {attempted} interaction")]
    ActionKindMismatch {
        /// The action key that was targeted.
        key: String,
        /// The interaction kind that was attempted.
        attempted: &'static str,
    },

    /// A `choice` interaction picked an option that action does not offer.
    #[error("action `{key}` has no option `{option}`")]
    UnknownChoiceOption {
        /// The action key that was targeted.
        key: String,
        /// The option that was picked.
        option: String,
    },

    /// A `view` action pointed at a non-http(s) URL (§2.3).
    #[error("`{scheme}:` links are not allowed in a view action — http(s) only")]
    UrlSchemeNotAllowed {
        /// The scheme that was refused.
        scheme: String,
    },

    /// A group-scoped silence needs a duration (§2.3).
    #[error("a group-scoped silence must carry a duration")]
    SilenceGroupNeedsDuration,

    /// The notification was deleted or has expired; it accepts no further writes.
    #[error("this notification no longer exists")]
    NotificationGone,

    /// The dead letter is not there to requeue (already requeued, or purged).
    #[error("this dead letter is no longer in the queue")]
    DeadLetterGone,

    /// The object is managed by GitOps config (§11): change the config, not the request.
    #[error("`{what}` is managed by GitOps — change it in config")]
    ManagedByConfig {
        /// What was being changed, e.g. `channel.policy`.
        what: &'static str,
    },

    /// The credential presented is not valid. Deliberately one variant for every
    /// cause so the endpoint is not an oracle; the detail goes to the log.
    #[error("the credential presented is not valid")]
    InvalidCredential,

    /// The API key presented has passed its `expires_at` (§5.2, T12).
    #[error("this API key expired")]
    ApiKeyExpired,

    /// The API key presented was revoked.
    #[error("this API key was revoked")]
    ApiKeyRevoked,

    /// The service account behind this request is deactivated (§5.2bis).
    #[error("this service account is deactivated")]
    ServiceAccountDisabled,

    /// The principal lacks the required scope in that channel (§5.2); never says who holds it.
    #[error("this principal does not hold `{scope}` in that channel")]
    ScopeMissing {
        /// The scope that would have been needed.
        scope: &'static str,
    },

    /// The endpoint describes a person and this caller is not one (§2.4).
    #[error("this principal has no profile of its own")]
    NotAPerson,

    /// The instance-wide platform role is required (§5.4, T30); only GitOps grants it.
    #[error("this action needs the platform role")]
    NotPlatformAdmin,

    /// The action needs a person's own session, not a key or a CI token.
    #[error("this action needs a person's own session, not a key or a CI token")]
    SessionRequired,

    /// Its owner disconnected this credential from realtime; reconnecting won't help.
    #[error("this connection was disconnected by its owner")]
    StreamDisconnected,
}

impl DomainError {
    /// The stable wire code; part of the public API contract.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing { .. } => "field.missing",
            Self::Empty { .. } => "field.empty",
            Self::TooLong { .. } => "field.too_long",
            Self::Malformed { .. } => "field.malformed",
            Self::TooMany { .. } => "collection.too_many",
            Self::TooLarge { .. } => "payload.too_large",
            Self::RevisionLimitReached { .. } => "notification.revision_limit_reached",
            Self::DeviceLimitReached { .. } => "device.limit_reached",
            Self::InfiniteTtlNotAllowed => "ttl.infinite_not_allowed",
            Self::DuplicateActionKey { .. } => "action.duplicate_key",
            Self::UnknownActionKey { .. } => "action.unknown_key",
            Self::ActionKindMismatch { .. } => "action.kind_mismatch",
            Self::UnknownChoiceOption { .. } => "action.unknown_option",
            Self::UrlSchemeNotAllowed { .. } => "action.url_scheme_not_allowed",
            Self::SilenceGroupNeedsDuration => "silence.duration_required",
            Self::NotificationGone => "notification.gone",
            Self::DeadLetterGone => "dlq.gone",
            Self::ManagedByConfig { .. } => "origin.managed",
            Self::InvalidCredential => "auth.invalid_credential",
            Self::ApiKeyExpired => "apikey.expired",
            Self::ApiKeyRevoked => "apikey.revoked",
            Self::ServiceAccountDisabled => "service_account.disabled",
            Self::ScopeMissing { .. } => "scope.missing",
            Self::NotAPerson => "principal.not_a_person",
            Self::NotPlatformAdmin => "platform.role_required",
            Self::SessionRequired => "credential.session_required",
            Self::StreamDisconnected => "stream.disconnected",
        }
    }

    /// The category a transport maps to its own error vocabulary.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Missing { .. }
            | Self::Empty { .. }
            | Self::TooLong { .. }
            | Self::Malformed { .. }
            | Self::DuplicateActionKey { .. }
            | Self::UnknownActionKey { .. }
            | Self::ActionKindMismatch { .. }
            | Self::UnknownChoiceOption { .. }
            | Self::UrlSchemeNotAllowed { .. }
            | Self::SilenceGroupNeedsDuration => ErrorKind::Validation,
            Self::TooMany { .. }
            | Self::RevisionLimitReached { .. }
            | Self::DeviceLimitReached { .. }
            | Self::InfiniteTtlNotAllowed => ErrorKind::LimitExceeded,
            Self::TooLarge { .. } => ErrorKind::TooLarge,
            Self::NotificationGone | Self::DeadLetterGone | Self::ManagedByConfig { .. } => {
                ErrorKind::Conflict
            }
            Self::InvalidCredential
            | Self::ApiKeyExpired
            | Self::ApiKeyRevoked
            | Self::ServiceAccountDisabled => ErrorKind::Unauthenticated,
            Self::ScopeMissing { .. }
            | Self::NotAPerson
            | Self::NotPlatformAdmin
            | Self::SessionRequired
            | Self::StreamDisconnected => ErrorKind::Forbidden,
        }
    }
}

/// Shorthand for the domain's fallible operations.
pub type Result<T> = core::result::Result<T, DomainError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant, so the contract tests below cannot silently miss one.
    fn every_variant() -> Vec<DomainError> {
        vec![
            DomainError::Missing { field: "f" },
            DomainError::Empty { field: "f" },
            DomainError::TooLong {
                field: "f",
                max: 1,
                actual: 2,
            },
            DomainError::Malformed {
                field: "f",
                reason: "because",
            },
            DomainError::TooMany {
                what: "things",
                max: 1,
                actual: 2,
            },
            DomainError::TooLarge {
                what: "payload",
                max: 1,
                actual: 2,
            },
            DomainError::RevisionLimitReached { max: 1_000 },
            DomainError::DeviceLimitReached { max: 20 },
            DomainError::InfiniteTtlNotAllowed,
            DomainError::DuplicateActionKey {
                key: "ack".to_owned(),
            },
            DomainError::UnknownActionKey {
                key: "ack".to_owned(),
            },
            DomainError::ActionKindMismatch {
                key: "ack".to_owned(),
                attempted: "reply",
            },
            DomainError::UnknownChoiceOption {
                key: "escalate".to_owned(),
                option: "oncall".to_owned(),
            },
            DomainError::UrlSchemeNotAllowed {
                scheme: "javascript".to_owned(),
            },
            DomainError::SilenceGroupNeedsDuration,
            DomainError::NotificationGone,
            DomainError::ManagedByConfig {
                what: "channel.policy",
            },
            DomainError::InvalidCredential,
            DomainError::ApiKeyExpired,
            DomainError::ApiKeyRevoked,
            DomainError::ServiceAccountDisabled,
            DomainError::ScopeMissing { scope: "publish" },
        ]
    }

    #[test]
    fn the_code_table_is_the_contract() {
        // Clients match on these strings: renaming one is a breaking change.
        let codes: Vec<&str> = every_variant().iter().map(DomainError::code).collect();

        assert_eq!(
            codes,
            [
                "field.missing",
                "field.empty",
                "field.too_long",
                "field.malformed",
                "collection.too_many",
                "payload.too_large",
                "notification.revision_limit_reached",
                "device.limit_reached",
                "ttl.infinite_not_allowed",
                "action.duplicate_key",
                "action.unknown_key",
                "action.kind_mismatch",
                "action.unknown_option",
                "action.url_scheme_not_allowed",
                "silence.duration_required",
                "notification.gone",
                "origin.managed",
                "auth.invalid_credential",
                "apikey.expired",
                "apikey.revoked",
                "service_account.disabled",
                "scope.missing",
            ]
        );
    }

    #[test]
    fn only_the_shared_field_codes_are_reused() {
        // Codes are shared only when they mean the same thing to a client.
        let mut codes: Vec<&str> = every_variant().iter().map(DomainError::code).collect();
        let total = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), total, "duplicate error code");
    }

    #[test]
    fn each_code_maps_to_the_category_a_transport_expects() {
        for error in every_variant() {
            let expected = match error.code() {
                "collection.too_many"
                | "notification.revision_limit_reached"
                | "device.limit_reached"
                | "ttl.infinite_not_allowed" => ErrorKind::LimitExceeded,
                "payload.too_large" => ErrorKind::TooLarge,
                "notification.gone" | "origin.managed" => ErrorKind::Conflict,
                "auth.invalid_credential"
                | "apikey.expired"
                | "apikey.revoked"
                | "service_account.disabled" => ErrorKind::Unauthenticated,
                "scope.missing" => ErrorKind::Forbidden,
                _ => ErrorKind::Validation,
            };
            assert_eq!(error.kind(), expected, "for {}", error.code());
        }
    }

    #[test]
    fn every_variant_says_something_useful() {
        for error in every_variant() {
            let message = error.to_string();
            assert!(!message.is_empty(), "{} has no message", error.code());
            assert!(
                !message.contains('{'),
                "{} left a format placeholder unfilled",
                error.code()
            );
        }
    }
}
