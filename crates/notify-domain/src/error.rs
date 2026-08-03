//! The single error type the domain rejects with, and its stable wire code.
//!
//! Every rejection carries a `code` that is part of the public contract: it goes
//! into the `code` field of the `application/problem+json` body (design §8) and is
//! consumed *as an identifier* by the CLI, the TUI and the web client to pick a
//! user-facing message. Renaming a code is therefore a breaking API change —
//! adding one is not.
//!
//! The domain deliberately does not know about HTTP. It exposes a coarse
//! [`ErrorKind`] instead, and the HTTP adapter maps that to a status code, so the
//! mapping lives in exactly one place without dragging the web vocabulary into the
//! notification model.

use thiserror::Error;

/// The coarse category of a rejection, so a transport can map it to its own
/// vocabulary (HTTP status, exit code, gRPC status…) without matching on every
/// variant.
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

    /// A field was present but contained nothing once sanitised (design §2.3):
    /// a title made only of ANSI escapes is an empty title.
    #[error("`{field}` is empty once sanitised")]
    Empty {
        /// Dotted path of the offending field.
        field: &'static str,
    },

    /// A text field is longer than its documented ceiling. Measured in characters,
    /// not bytes — the limit users are told about is the one they can count.
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

    /// The 1 000-revision ceiling was hit (design §8.2): the notification stays
    /// intact and readable, only further revisions are refused.
    #[error("this notification already has {max} revisions — publish a new one")]
    RevisionLimitReached {
        /// Documented ceiling.
        max: u32,
    },

    /// `ttl = 0` means "keep forever", which a channel must opt into explicitly
    /// (design §7.2bis).
    #[error("infinite retention is not enabled on this channel")]
    InfiniteTtlNotAllowed,

    /// Two actions claim the same key, so an interaction on that key would be
    /// ambiguous.
    #[error("two actions share the key `{key}`")]
    DuplicateActionKey {
        /// The colliding key.
        key: String,
    },

    /// An interaction referenced an action the current revision does not offer.
    /// Expected after a revision replaced the action set (design §2.3).
    #[error("this notification has no action with the key `{key}`")]
    UnknownActionKey {
        /// The key that was looked up.
        key: String,
    },

    /// The interaction kind does not match the action kind, e.g. a reply on an
    /// `ack` button.
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

    /// A `view` action pointed somewhere other than the web (design §2.3): a
    /// notification must never be able to hand a client a `javascript:` or
    /// `file:` URL to open.
    #[error("`{scheme}:` links are not allowed in a view action — http(s) only")]
    UrlSchemeNotAllowed {
        /// The scheme that was refused.
        scheme: String,
    },

    /// A group-scoped silence with no duration would mute a whole group forever
    /// by accident (design §2.3).
    #[error("a group-scoped silence must carry a duration")]
    SilenceGroupNeedsDuration,

    /// The notification was deleted or has expired; it accepts no further writes.
    #[error("this notification no longer exists")]
    NotificationGone,

    /// The object is declared in GitOps config, which owns its shape (design §11).
    ///
    /// Its own variant rather than a [`Self::Malformed`] because the request is not
    /// malformed at all — it is understood, and refused by policy. That distinction
    /// is what a client branches on: `origin.managed` says "go and change the
    /// config", where `field.malformed` says "you typed it wrong".
    #[error("`{what}` is managed by GitOps — change it in config")]
    ManagedByConfig {
        /// What was being changed, e.g. `channel.policy`.
        what: &'static str,
    },

    /// The credential presented is not valid: a bad signature, an unknown key, a
    /// wrong secret, a malformed header.
    ///
    /// One variant for all of them on purpose. Telling a caller *which* turns the
    /// endpoint into an oracle — "unknown prefix" versus "wrong secret" says which
    /// guesses landed on a real key. The log records the detail; the response does
    /// not.
    #[error("the credential presented is not valid")]
    InvalidCredential,

    /// The API key presented has passed its `expires_at`.
    ///
    /// Distinct from [`Self::ApiKeyRevoked`] so a client can say "your key expired,
    /// here is how to rotate it" rather than a flat 401 (design §5.2, scenario T12).
    #[error("this API key expired")]
    ApiKeyExpired,

    /// The API key presented was revoked.
    #[error("this API key was revoked")]
    ApiKeyRevoked,

    /// The service account behind this request is deactivated (design §5.2bis).
    #[error("this service account is deactivated")]
    ServiceAccountDisabled,

    /// The principal is authenticated but does not hold the required scope in the
    /// channel it is reaching for (design §5.2).
    ///
    /// Deliberately says *what* was missing and not *who* holds it: membership of a
    /// channel you are not in is not information you get to enumerate.
    #[error("this principal does not hold `{scope}` in that channel")]
    ScopeMissing {
        /// The scope that would have been needed.
        scope: &'static str,
    },

    /// The endpoint describes a person, and this caller is not one (design §2.4).
    ///
    /// A service account has a profile of its own but no *settings* to edit, and a
    /// channel key is anonymous by construction (§5.2) — there is nobody to
    /// describe. Its own code rather than a scope refusal, because "you lack a
    /// right" and "there is no such thing to have" send a client down different
    /// paths: the first is worth asking an admin about, the second never will be.
    #[error("this principal has no profile of its own")]
    NotAPerson,
}

impl DomainError {
    /// The stable wire code. Part of the public API contract — see the module docs.
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
            Self::InfiniteTtlNotAllowed => "ttl.infinite_not_allowed",
            Self::DuplicateActionKey { .. } => "action.duplicate_key",
            Self::UnknownActionKey { .. } => "action.unknown_key",
            Self::ActionKindMismatch { .. } => "action.kind_mismatch",
            Self::UnknownChoiceOption { .. } => "action.unknown_option",
            Self::UrlSchemeNotAllowed { .. } => "action.url_scheme_not_allowed",
            Self::SilenceGroupNeedsDuration => "silence.duration_required",
            Self::NotificationGone => "notification.gone",
            Self::ManagedByConfig { .. } => "origin.managed",
            Self::InvalidCredential => "auth.invalid_credential",
            Self::ApiKeyExpired => "apikey.expired",
            Self::ApiKeyRevoked => "apikey.revoked",
            Self::ServiceAccountDisabled => "service_account.disabled",
            Self::ScopeMissing { .. } => "scope.missing",
            Self::NotAPerson => "principal.not_a_person",
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
            | Self::InfiniteTtlNotAllowed => ErrorKind::LimitExceeded,
            Self::TooLarge { .. } => ErrorKind::TooLarge,
            Self::NotificationGone | Self::ManagedByConfig { .. } => ErrorKind::Conflict,
            Self::InvalidCredential
            | Self::ApiKeyExpired
            | Self::ApiKeyRevoked
            | Self::ServiceAccountDisabled => ErrorKind::Unauthenticated,
            Self::ScopeMissing { .. } | Self::NotAPerson => ErrorKind::Forbidden,
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
        // Clients match on these strings to choose what to tell a user. Renaming
        // one is a breaking API change, so it has to be a deliberate edit here
        // rather than a rename that quietly rides along with a refactor.
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
        // Two rejections may share a code only when they genuinely mean the same
        // thing to a client — "some field was wrong" carries its detail in the
        // message, not in the code.
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
