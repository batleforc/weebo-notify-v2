//! Port failures: [`PortError`] means the world was wrong (5xx), [`DomainError`](notify_domain::DomainError)
//! the request (4xx). Adapters map their own errors here, keeping them as `source`.

use thiserror::Error;

/// A port failed for infrastructure reasons.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PortError {
    /// The backing store, transport or service could not be reached.
    #[error("{what} is unavailable")]
    Unavailable {
        /// Which dependency, as named by `/readyz` and metrics (`postgres`, `jwks`, …).
        what: &'static str,
        /// The underlying failure.
        #[source]
        source: Box<dyn core::error::Error + Send + Sync>,
    },

    /// A concurrent writer won a race (unique index, optimistic update); often
    /// recoverable by re-reading at the use-case level.
    #[error("{what} conflicted with a concurrent write")]
    Conflict {
        /// What collided, e.g. `notification.external_id`.
        what: &'static str,
    },

    /// Something this deployment owns cannot be read back (old row, bad key file).
    /// Always ours, never the client's.
    #[error("stored {what} could not be read back: {detail}")]
    Corrupt {
        /// Which record, e.g. `notification.payload`.
        what: &'static str,
        /// What went wrong, for the operator reading the log.
        detail: String,
    },

    /// The port is not configured on this instance (e.g. an absent push transport).
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

    /// The dependency name to report on `/readyz` and in metrics (§13), if any.
    #[must_use]
    pub const fn dependency(&self) -> Option<&'static str> {
        match self {
            Self::Unavailable { what, .. } => Some(what),
            Self::Conflict { .. } | Self::Corrupt { .. } | Self::NotConfigured { .. } => None,
        }
    }

    /// Whether retrying the exact same call could plausibly succeed (§7.1bis).
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable { .. } | Self::Conflict { .. })
    }
}

/// Shorthand for a port's fallible operations.
pub type PortResult<T> = Result<T, PortError>;

/// A use case's failure: the caller's fault, the world's, or neither; one type
/// for handlers to map onto `application/problem+json` (§8).
#[derive(Debug, Error)]
pub enum Failure {
    /// The request was wrong. Carries the stable `code` clients match on.
    #[error(transparent)]
    Domain(#[from] notify_domain::DomainError),
    /// The world was wrong.
    #[error(transparent)]
    Port(#[from] PortError),
    /// Too much traffic (§8.2): a `429` the client is expected to retry.
    #[error("rate limit exceeded; retry in {retry_after_seconds}s")]
    RateLimited {
        /// What to put in `Retry-After`.
        retry_after_seconds: u32,
    },
}

impl Failure {
    /// The stable wire code, for the failures that have one (infrastructure has none).
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
