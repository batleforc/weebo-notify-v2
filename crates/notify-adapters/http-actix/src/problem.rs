//! Turning a [`Failure`] into an HTTP response (design §8, RFC 9457).

use actix_web::http::header::{self, HeaderValue};
use actix_web::{HttpResponse, ResponseError};
use notify_domain::ErrorKind;
use notify_ports::{Failure, PortError};
use serde::Serialize;
use utoipa::ToSchema;

/// The `application/problem+json` body every refusal carries; clients match on `code`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Problem {
    /// Always `about:blank`; `code` carries the specificity.
    #[serde(rename = "type")]
    #[schema(example = "about:blank")]
    pub kind: &'static str,
    /// A short, human-readable summary. Prose, and subject to change.
    pub title: String,
    /// The HTTP status, repeated in the body as RFC 9457 asks.
    #[schema(example = 422)]
    pub status: u16,
    /// The stable code to match on; absent only for server-side failures.
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
                // `Corrupt` and any future variant: ours, the safe reading.
                _ => Code::INTERNAL_SERVER_ERROR,
            },
        }
    }

    /// The body, with the detail an operator needs kept out of it.
    fn problem(&self) -> Problem {
        let status = self.status();

        let title = match &self.0 {
            Failure::Domain(error) => error.to_string(),
            Failure::RateLimited { .. } => "too many requests".to_owned(),
            // About us: the cause goes to the log below, not to the caller.
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

        // Logged here so no 5xx goes out without its cause in the log.
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
            response.insert_header((
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"weebo-notify\""),
            ));
        }

        response.json(problem)
    }
}

/// The largest JSON body any route reads.
///
/// Four payload ceilings: a publish carries at most [`MAX_PAYLOAD_BYTES`] of
/// content, and JSON escaping can grow that several-fold before the domain measures
/// it. Anything bigger is refused before it is buffered, rather than at actix's 2 MiB
/// default.
///
/// [`MAX_PAYLOAD_BYTES`]: notify_domain::limits::MAX_PAYLOAD_BYTES
pub(crate) const MAX_JSON_BODY_BYTES: usize = 4 * notify_domain::limits::MAX_PAYLOAD_BYTES;

/// Actix's own extractor refusals, in the same `problem+json` as every other one.
///
/// Its defaults answer `text/plain` quoting serde's description of our types. A
/// client that matches on `code` would get nothing to match, and the detail —
/// logged here at debug — is ours rather than the caller's.
fn extractor_refusal(field: &'static str, error: &dyn core::fmt::Display) -> actix_web::Error {
    tracing::debug!(%error, field, "request refused by an extractor");
    ApiError::from(notify_domain::DomainError::Malformed {
        field,
        reason: "could not be read for this route",
    })
    .into()
}

/// [`actix_web::web::JsonConfig`] with an explicit ceiling and a `problem+json` refusal.
pub(crate) fn json_config() -> actix_web::web::JsonConfig {
    use actix_web::error::JsonPayloadError;

    actix_web::web::JsonConfig::default()
        .limit(MAX_JSON_BODY_BYTES)
        .error_handler(|error, _| match &error {
            JsonPayloadError::Overflow { .. } | JsonPayloadError::OverflowKnownLength { .. } => {
                ApiError::from(notify_domain::DomainError::TooLarge {
                    what: "body",
                    max: MAX_JSON_BODY_BYTES,
                    actual: match error {
                        JsonPayloadError::OverflowKnownLength { length, .. } => length,
                        _ => 0,
                    },
                })
                .into()
            }
            _ => extractor_refusal("body", &error),
        })
}

/// [`actix_web::web::QueryConfig`] with a `problem+json` refusal.
pub(crate) fn query_config() -> actix_web::web::QueryConfig {
    actix_web::web::QueryConfig::default()
        .error_handler(|error, _| extractor_refusal("query", &error))
}

/// [`actix_web::web::PathConfig`] with a `problem+json` refusal.
pub(crate) fn path_config() -> actix_web::web::PathConfig {
    actix_web::web::PathConfig::default()
        .error_handler(|error, _| extractor_refusal("path", &error))
}

#[cfg(test)]
mod tests {
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

    #[actix_web::test]
    async fn a_body_actix_cannot_read_is_a_problem_not_plain_text() {
        use actix_web::{App, test, web};

        #[derive(serde::Deserialize)]
        struct Body {
            #[expect(dead_code, reason = "only its absence matters")]
            title: String,
        }

        let app = test::init_service(
            App::new()
                .app_data(json_config())
                .route("/", web::post().to(|_: web::Json<Body>| async { "ok" })),
        )
        .await;

        for body in [
            "{\"titel\": 1}".to_owned(),
            format!("{{\"title\": \"{}\"}}", "x".repeat(MAX_JSON_BODY_BYTES)),
        ] {
            let response = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/")
                    .insert_header((header::CONTENT_TYPE, "application/json"))
                    .set_payload(body)
                    .to_request(),
            )
            .await;

            assert!(response.status().is_client_error());
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                "application/problem+json"
            );
            let text = test::read_body(response).await;
            assert!(
                !String::from_utf8_lossy(&text).contains("titel"),
                "serde's detail reached the caller"
            );
        }
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
