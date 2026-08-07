//! What a client call can fail with, in terms a front end can act on.

use notify_client_api::types::Problem;

/// A failed call.
///
/// The distinction that earns its place is [`Self::Refused`] versus the rest: a
/// refusal carries the server's stable `code` (design §8), which is what a script
/// branches on and what a UI turns into a sentence. Everything else is "we never
/// got an answer", and no amount of parsing changes what the caller should do.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server answered, and said no.
    #[error("{}", refusal(.0))]
    Refused(Box<Problem>),

    /// The server answered with a status the contract does not describe.
    #[error("the server answered {status} unexpectedly")]
    Unexpected {
        /// What it answered.
        status: u16,
    },

    /// The server could not be reached, or the answer could not be read.
    #[error("could not reach the server: {0}")]
    Transport(#[source] reqwest::Error),

    /// The configured token cannot be put in a header — a stray newline, usually
    /// from a shell that captured more than the value.
    #[error("the configured token is not a usable credential")]
    BadCredential,

    /// A value this client was handed cannot be used.
    ///
    /// Distinct from [`ClientError::Refused`], which is the *server* saying no: this
    /// is the client refusing to act on something it was given — a preference naming
    /// a timezone that is not in this build's IANA database, most often. The detail
    /// is meant to be shown, because the person reading it is the one who can fix
    /// the value.
    #[error("{detail}")]
    Invalid {
        /// What was wrong with it, phrased for whoever has to correct it.
        detail: String,
    },

    /// The local store could not be opened, read or written (design §9.3bis).
    ///
    /// Its own variant because it is the one failure that has nothing to do with
    /// the server: a full disk, a data directory somebody made read-only, a store
    /// written by a newer client. The detail is meant to be shown — "cannot open
    /// /home/…/store.db: permission denied" is something the person reading it can
    /// act on, which is not true of most error text.
    #[error("the local store is unusable: {detail}")]
    Store {
        /// What went wrong, phrased for whoever has to fix it.
        detail: String,
    },
}

impl ClientError {
    /// The server's stable code, when there is one.
    ///
    /// Scripts match on this, not on the message: the message is prose and gets
    /// reworded (design §8).
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Refused(problem) => problem.code.as_deref(),
            _ => None,
        }
    }

    /// Whether trying the exact same call again could work.
    ///
    /// A rate limit says yes and says when; a validation failure says no however
    /// many times it is asked.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Refused(problem) => problem.status >= 500 || problem.status == 429,
            Self::Transport(_) => true,
            Self::Unexpected { status } => *status >= 500,
            // None is fixed by asking again: a token with a newline in it stays
            // unusable, a full disk stays full, and a timezone that is not one will
            // not become one on the second attempt.
            Self::BadCredential | Self::Invalid { .. } | Self::Store { .. } => false,
        }
    }
}

fn refusal(problem: &Problem) -> String {
    match &problem.code {
        Some(code) => format!("{} ({code})", problem.title),
        None => problem.title.clone(),
    }
}

impl From<progenitor_client::Error<Problem>> for ClientError {
    fn from(error: progenitor_client::Error<Problem>) -> Self {
        use progenitor_client::Error as Generated;

        match error {
            // The one case with something to say: a `problem+json` body.
            Generated::ErrorResponse(response) => Self::Refused(Box::new(response.into_inner())),
            Generated::UnexpectedResponse(response) => Self::Unexpected {
                status: response.status().as_u16(),
            },
            Generated::CommunicationError(source) | Generated::ResponseBodyError(source) => {
                Self::Transport(source)
            }
            // Invalid requests and undocumented statuses are our bug rather than the
            // server's, but the caller can only be told the call did not land.
            other => Self::Unexpected {
                status: other.status().map_or(0, |status| status.as_u16()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(status: i32, code: Option<&str>) -> Problem {
        Problem {
            type_: "about:blank".to_owned(),
            title: "the payload does not validate".to_owned(),
            status,
            code: code.map(ToOwned::to_owned),
            retry_after_seconds: None,
        }
    }

    #[test]
    fn a_refusal_keeps_the_code_a_script_branches_on() {
        let error = ClientError::Refused(Box::new(problem(422, Some("field.empty"))));

        assert_eq!(error.code(), Some("field.empty"));
        assert!(error.to_string().contains("field.empty"));
        assert!(!error.is_retryable());
    }

    #[test]
    fn what_is_worth_retrying_says_so() {
        assert!(ClientError::Refused(Box::new(problem(429, Some("rate.limited")))).is_retryable());
        assert!(ClientError::Refused(Box::new(problem(503, None))).is_retryable());
        assert!(
            !ClientError::Refused(Box::new(problem(403, Some("scope.missing")))).is_retryable()
        );
        assert!(!ClientError::BadCredential.is_retryable());
    }

    #[test]
    fn a_refusal_with_no_code_still_reads_as_a_sentence() {
        let error = ClientError::Refused(Box::new(problem(503, None)));

        assert_eq!(error.to_string(), "the payload does not validate");
    }
}
