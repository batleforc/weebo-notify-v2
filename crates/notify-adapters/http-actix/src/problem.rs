//! Turning a [`Failure`] into an HTTP response (design §8, RFC 9457).

use actix_web::http::header::{self, HeaderValue};
use actix_web::{HttpResponse, ResponseError};
use notify_domain::ErrorKind;
use notify_ports::{Failure, PortError};
use serde::Serialize;
use utoipa::ToSchema;

/// The `application/problem+json` body every refusal carries.
///
/// The field clients are told to match on is `code`, not `status` and certainly not
/// `title` — statuses are coarse and titles are prose that gets reworded. A code is
/// a promise (design §8, `notify-domain`'s error table).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Problem {
    /// Always `about:blank` for now: the `code` field carries the specificity a
    /// `type` URI usually would, and inventing URIs nobody dereferences is
    /// ceremony.
    #[serde(rename = "type")]
    #[schema(example = "about:blank")]
    pub kind: &'static str,
    /// A short, human-readable summary. Prose, and subject to change.
    pub title: String,
    /// The HTTP status, repeated in the body as RFC 9457 asks.
    #[schema(example = 422)]
    pub status: u16,
    /// The stable code. Absent only when the failure is ours rather than the
    /// caller's — there is nothing useful to branch on when a database is down.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "field.missing")]
    pub code: Option<&'static str>,
    /// When to come back, on a `429`. Mirrors the `Retry-After` header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u32>,
}

/// A [`Failure`] on its way out of a handler.
#[derive(Debug)]
pub struct ApiError(Failure);

impl From<Failure> for ApiError {
    fn from(failure: Failure) -> Self {
        Self(failure)
    }
}

impl From<notify_domain::DomainError> for ApiError {
    fn from(error: notify_domain::DomainError) -> Self {
        Self(Failure::Domain(error))
    }
}

impl From<PortError> for ApiError {
    fn from(error: PortError) -> Self {
        Self(Failure::Port(error))
    }
}

impl core::fmt::Display for ApiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl ApiError {
    /// The status this failure answers with.
    #[must_use]
    pub fn status(&self) -> actix_web::http::StatusCode {
        use actix_web::http::StatusCode as Code;

        match &self.0 {
            Failure::Domain(error) => match error.kind() {
                ErrorKind::Validation | ErrorKind::LimitExceeded => Code::UNPROCESSABLE_ENTITY,
                ErrorKind::TooLarge => Code::PAYLOAD_TOO_LARGE,
                ErrorKind::Conflict => Code::CONFLICT,
                ErrorKind::Unauthenticated => Code::UNAUTHORIZED,
                ErrorKind::Forbidden => Code::FORBIDDEN,
            },
            Failure::RateLimited { .. } => Code::TOO_MANY_REQUESTS,
            Failure::Port(error) => match error {
                PortError::Unavailable { .. } => Code::SERVICE_UNAVAILABLE,
                PortError::Conflict { .. } => Code::CONFLICT,
                PortError::NotConfigured { .. } => Code::NOT_IMPLEMENTED,
                // `Corrupt`, plus whatever `PortError` grows next: an adapter that
                // cannot classify a failure reports it as ours, which is the safe
                // reading and keeps a new variant from being a compile error here.
                _ => Code::INTERNAL_SERVER_ERROR,
            },
        }
    }

    /// The body, with the detail an operator needs kept out of it.
    fn problem(&self) -> Problem {
        let status = self.status();

        let title = match &self.0 {
            // A domain refusal is about the request, so the caller gets to read it:
            // "`payload.title` is required" is exactly what they need.
            Failure::Domain(error) => error.to_string(),
            Failure::RateLimited { .. } => "too many requests".to_owned(),
            // A port failure is about us. The caller gets a shrug and the operator
            // gets the cause, in the log line emitted below.
            Failure::Port(_) => "the request could not be served".to_owned(),
        };

        Problem {
            kind: "about:blank",
            title,
            status: status.as_u16(),
            code: self.0.code(),
            retry_after_seconds: match &self.0 {
                Failure::RateLimited {
                    retry_after_seconds,
                } => Some(*retry_after_seconds),
                _ => None,
            },
        }
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> actix_web::http::StatusCode {
        self.status()
    }

    fn error_response(&self) -> HttpResponse {
        let problem = self.problem();

        // Logged here rather than at every call site, so nothing can return a 5xx
        // without the cause landing somewhere an operator will find it.
        match &self.0 {
            Failure::Port(error) => {
                tracing::error!(
                    error = %error,
                    dependency = error.dependency().unwrap_or("-"),
                    "serving a failure the caller cannot do anything about"
                );
            }
            Failure::Domain(error) => tracing::debug!(code = error.code(), "refused"),
            Failure::RateLimited { .. } => tracing::info!("rate limited"),
        }

        let mut response = HttpResponse::build(self.status());
        response.insert_header((header::CONTENT_TYPE, "application/problem+json"));

        if let Some(seconds) = problem.retry_after_seconds {
            response.insert_header((header::RETRY_AFTER, seconds));
        }
        if self.status() == actix_web::http::StatusCode::UNAUTHORIZED {
            // Without this a 401 is not a well-formed challenge, and a client
            // library cannot tell "log in" from "you may not".
            response.insert_header((
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"weebo-notify\""),
            ));
        }

        response.json(problem)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::DomainError;

    use super::*;

    fn problem_of(failure: Failure) -> (u16, Problem) {
        let error = ApiError(failure);
        (error.status().as_u16(), error.problem())
    }

    #[test]
    fn a_validation_refusal_is_a_422_carrying_its_code() {
        let (status, problem) = problem_of(DomainError::Missing { field: "title" }.into());

        assert_eq!(status, 422);
        assert_eq!(problem.code, Some("field.missing"));
        assert!(problem.title.contains("title"));
    }

    #[test]
    fn each_kind_of_refusal_lands_on_its_own_status() {
        let cases: Vec<(Failure, u16)> = vec![
            (DomainError::InvalidCredential.into(), 401),
            (DomainError::ScopeMissing { scope: "publish" }.into(), 403),
            (DomainError::NotificationGone.into(), 409),
            (
                DomainError::TooLarge {
                    what: "payload",
                    max: 1,
                    actual: 2,
                }
                .into(),
                413,
            ),
            (
                Failure::RateLimited {
                    retry_after_seconds: 30,
                },
                429,
            ),
            (
                PortError::unavailable("postgres", std::io::Error::other("down")).into(),
                503,
            ),
        ];

        for (failure, expected) in cases {
            let (status, _) = problem_of(failure);
            assert_eq!(status, expected);
        }
    }

    #[test]
    fn an_infrastructure_failure_tells_the_caller_nothing_about_our_insides() {
        let (status, problem) = problem_of(
            PortError::unavailable(
                "postgres",
                std::io::Error::other("password authentication failed for user weebo"),
            )
            .into(),
        );

        assert_eq!(status, 503);
        assert!(
            !problem.title.contains("password"),
            "the connection string must not reach the caller: {}",
            problem.title
        );
        assert_eq!(
            problem.code, None,
            "a client cannot branch usefully on our database being unhappy"
        );
    }

    #[test]
    fn a_rate_limit_says_when_to_come_back_in_the_body_and_the_header() {
        let error = ApiError(Failure::RateLimited {
            retry_after_seconds: 42,
        });
        let response = error.error_response();

        assert_eq!(response.status().as_u16(), 429);
        assert_eq!(
            response.headers().get(header::RETRY_AFTER).unwrap(),
            "42",
            "a 429 without Retry-After turns a polite client into a busy loop"
        );
    }

    #[test]
    fn a_401_is_a_well_formed_challenge() {
        let error = ApiError(DomainError::InvalidCredential.into());
        let response = error.error_response();

        assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/problem+json"
        );
    }
}
