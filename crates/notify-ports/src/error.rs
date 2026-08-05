//! What a port is allowed to fail with.
//!
//! The split that matters: a [`DomainError`] means *the request was wrong*, a
//! [`PortError`] means *the world was wrong*. One is a 4xx a publisher can fix, the
//! other is a 5xx an operator has to look at. Keeping them in separate types stops
//! a use case from accidentally reporting a dropped database connection as a
//! validation failure, which is the kind of mistake that turns a page at 3am into
//! an hour of reading the wrong logs.
//!
//! Adapters therefore never surface their own error types. `sqlx::Error`,
//! `reqwest::Error` and friends stop at the adapter boundary and become one of
//! these — with the original attached as a `source`, so the detail survives into
//! the logs without leaking into the signature.

use thiserror::Error;

/// A port failed for infrastructure reasons.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PortError {
    /// The backing store, transport or service could not be reached.
    #[error("{what} is unavailable")]
    Unavailable {
        /// Which dependency, in the vocabulary used by `/readyz` and the metrics:
        /// `postgres`, `attachments`, `eventbus`, `jwks`.
        what: &'static str,
        /// The underlying failure.
        #[source]
        source: Box<dyn core::error::Error + Send + Sync>,
    },

    /// A concurrent writer won a race that this operation cannot simply retry
    /// through — a unique index collided, an optimistic update matched no rows.
    ///
    /// Distinct from [`Self::Unavailable`] because it is often *recoverable at the
    /// use-case level*: an idempotent upsert that loses this race can re-read and
    /// carry on rather than fail the request.
    #[error("{what} conflicted with a concurrent write")]
    Conflict {
        /// What collided, e.g. `notification.external_id`.
        what: &'static str,
    },

    /// Something this deployment owns cannot be read back: a row written by an older
    /// build, a column holding something no current type accepts, or a key file the
    /// operator mounted that no loader here accepts (design §5.3, §11).
    ///
    /// Never a client's fault, always ours — which is why it is here and not in
    /// [`DomainError`], even though the immediate cause is usually a failed
    /// validation on read.
    #[error("stored {what} could not be read back: {detail}")]
    Corrupt {
        /// Which record, e.g. `notification.payload`.
        what: &'static str,
        /// What went wrong, for the operator reading the log.
        detail: String,
    },

    /// The port was asked for something it does not implement in this deployment —
    /// a push transport that is not configured, an event bus backend that is off.
    ///
    /// A configuration answer, not a bug: it says "this instance does not do that",
    /// which an operator fixes in config rather than in code.
    #[error("{what} is not configured on this instance")]
    NotConfigured {
        /// The capability that was asked for, e.g. `push.apns`.
        what: &'static str,
    },
}

impl PortError {
    /// Wrap an adapter's own error as an availability failure.
    pub fn unavailable<E>(what: &'static str, source: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self::Unavailable {
            what,
            source: Box::new(source),
        }
    }

    /// Report stored data the domain refuses to accept back.
    #[must_use]
    pub fn corrupt(what: &'static str, detail: impl core::fmt::Display) -> Self {
        Self::Corrupt {
            what,
            detail: detail.to_string(),
        }
    }

    /// The dependency name to report on `/readyz` and in metrics (design §13), when
    /// this failure names one.
    #[must_use]
    pub const fn dependency(&self) -> Option<&'static str> {
        match self {
            Self::Unavailable { what, .. } => Some(what),
            Self::Conflict { .. } | Self::Corrupt { .. } | Self::NotConfigured { .. } => None,
        }
    }

    /// Whether retrying the exact same call could plausibly succeed.
    ///
    /// Read by the sink retry loop (design §7.1bis) and by the use cases that can
    /// re-read after losing an upsert race. `Corrupt` and `NotConfigured` say no:
    /// retrying a bad row or a missing configuration is a busy loop.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable { .. } | Self::Conflict { .. })
    }
}

/// Shorthand for a port's fallible operations.
pub type PortResult<T> = Result<T, PortError>;

/// A failure that is either the caller's fault, the world's, or neither.
///
/// The one type a use case returns, so a handler has a single thing to map onto
/// `application/problem+json` (design §8).
#[derive(Debug, Error)]
pub enum Failure {
    /// The request was wrong. Carries the stable `code` clients match on.
    #[error(transparent)]
    Domain(#[from] notify_domain::DomainError),
    /// The world was wrong.
    #[error(transparent)]
    Port(#[from] PortError),
    /// Neither: the request is fine and the world is fine, there has just been too
    /// much of it (design §8.2). Its own variant because `429` is the one refusal a
    /// client is expected to *retry*, and burying it in either of the other two
    /// would lose that.
    #[error("rate limit exceeded; retry in {retry_after_seconds}s")]
    RateLimited {
        /// What to put in `Retry-After`.
        retry_after_seconds: u32,
    },
}

impl Failure {
    /// The stable wire code, for the failures that have one.
    ///
    /// Infrastructure failures deliberately have none: a client cannot do anything
    /// differently about a database being down, and giving it a code to branch on
    /// would invite exactly that.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Domain(error) => Some(error.code()),
            Self::RateLimited { .. } => Some("rate.limited"),
            Self::Port(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_infrastructure_failure_keeps_its_cause_and_names_its_dependency() {
        let io = std::io::Error::other("connection reset");
        let error = PortError::unavailable("postgres", io);

        assert_eq!(error.dependency(), Some("postgres"));
        assert!(error.is_retryable());
        assert!(
            core::error::Error::source(&error).is_some(),
            "the operator reading the log needs the original"
        );
    }

    #[test]
    fn what_cannot_be_retried_says_so() {
        assert!(!PortError::corrupt("notification.payload", "bad title").is_retryable());
        assert!(!PortError::NotConfigured { what: "push.apns" }.is_retryable());
        assert!(
            PortError::Conflict {
                what: "external_id"
            }
            .is_retryable()
        );
    }

    #[test]
    fn only_the_callers_fault_carries_a_code() {
        let domain: Failure = notify_domain::DomainError::NotificationGone.into();
        assert_eq!(domain.code(), Some("notification.gone"));

        let port: Failure = PortError::Conflict { what: "x" }.into();
        assert_eq!(
            port.code(),
            None,
            "a client cannot branch usefully on our database being unhappy"
        );
    }
}
