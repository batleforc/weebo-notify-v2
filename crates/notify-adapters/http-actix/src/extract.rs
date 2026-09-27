//! Getting from a request to a [`Caller`], and from query strings to domain values.

use std::future::Future;
use std::pin::Pin;

use actix_web::http::header;
use actix_web::{FromRequest, HttpRequest, dev::Payload, web};
use notify_app::{Caller, Credential};
use notify_domain::{DomainError, PresentedApiKey};
use notify_ports::{PageRequest, PortError};
use serde::de::DeserializeOwned;

use crate::problem::ApiError;
use crate::state::ApiState;

/// An authenticated caller, extracted from `Authorization`.
///
/// Every handler that touches anything takes one. There is no "optional auth" path
/// in M1: an endpoint that can be reached anonymously is a decision, and none of
/// these are it.
#[derive(Debug, Clone)]
pub struct Authenticated(pub Caller);

impl FromRequest for Authenticated {
    type Error = ApiError;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let state = req.app_data::<web::Data<ApiState>>().cloned();
        let presented = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);

        Box::pin(async move {
            let Some(state) = state else {
                // Only reachable if the composition root forgot the app data, which
                // is a wiring bug rather than anything the caller did.
                return Err(PortError::NotConfigured { what: "http.state" }.into());
            };
            let credential = credential_from(presented.as_deref())?;
            Ok(Self(state.authenticate.execute(credential).await?))
        })
    }
}

/// Read the `Authorization` header.
///
/// Both credentials arrive as `Bearer`, because that is what every client and every
/// `curl` example already does (design §8). They are told apart by the key format
/// rather than by a second scheme: `wnk_` is a prefix we mint, so the test is exact
/// and a JWT can never be mistaken for a key.
pub(crate) fn credential_from(header: Option<&str>) -> Result<Credential, DomainError> {
    let raw = header.ok_or(DomainError::InvalidCredential)?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .ok_or(DomainError::InvalidCredential)?
        .trim();

    if token.is_empty() {
        return Err(DomainError::InvalidCredential);
    }

    if token.starts_with(PresentedApiKey::SCHEME) {
        Ok(Credential::ApiKey(token.to_owned()))
    } else {
        Ok(Credential::Bearer(token.to_owned()))
    }
}

/// Parse a value the domain deserialises from a string — a lifecycle state, a
/// status, a priority.
///
/// Going through serde rather than a hand-written match means the query string
/// accepts exactly what the JSON body accepts, which is one fewer place for
/// `in_progress` versus `working` to diverge.
pub(crate) fn parse_enum<T: DeserializeOwned>(
    raw: &str,
    field: &'static str,
) -> Result<T, DomainError> {
    serde_json::from_value(serde_json::Value::String(raw.to_owned())).map_err(|_| {
        DomainError::Malformed {
            field,
            reason: "not one of the documented values",
        }
    })
}

/// Split a comma-separated query parameter, dropping the empties.
///
/// `?channels=` with nothing after it means "no filter", not "one channel whose
/// name is the empty string".
pub(crate) fn csv(raw: Option<&str>) -> Vec<&str> {
    raw.map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// Build a page request from `?cursor=&limit=`.
pub(crate) fn page(cursor: Option<&str>, limit: Option<u32>) -> Result<PageRequest, DomainError> {
    let after = cursor
        .map(str::parse::<notify_domain::Cursor>)
        .transpose()?;
    let request = limit.map_or_else(PageRequest::first, PageRequest::of_size);

    Ok(match after {
        Some(cursor) => request.after(cursor),
        None => request,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::State;

    use super::*;

    #[test]
    fn a_key_and_a_token_arrive_the_same_way_and_are_told_apart_by_shape() {
        let key = credential_from(Some("Bearer wnk_abcd1234_thesecret")).unwrap();
        assert!(matches!(key, Credential::ApiKey(_)));

        let token = credential_from(Some("Bearer eyJhbGciOiJFUzI1NiJ9.e30.sig")).unwrap();
        assert!(matches!(token, Credential::Bearer(_)));
    }

    #[test]
    fn anything_that_is_not_a_bearer_header_is_simply_no_credential() {
        for header in [
            None,
            Some(""),
            Some("Bearer"),
            Some("Bearer   "),
            Some("Basic dXNlcjpwdw=="),
        ] {
            assert!(
                credential_from(header).is_err(),
                "unexpectedly accepted {header:?}"
            );
        }
    }

    #[test]
    fn the_scheme_is_matched_case_insensitively_because_clients_vary() {
        assert!(credential_from(Some("bearer wnk_abcd1234_secret")).is_ok());
    }

    #[test]
    fn query_enums_accept_exactly_what_the_body_accepts() {
        assert_eq!(
            parse_enum::<State>("closed", "state").unwrap(),
            State::Closed
        );
        assert!(parse_enum::<State>("finished", "state").is_err());
    }

    #[test]
    fn an_empty_csv_parameter_is_no_filter_rather_than_one_empty_value() {
        assert!(csv(None).is_empty());
        assert!(csv(Some("")).is_empty());
        assert!(csv(Some(" , ,")).is_empty());
        assert_eq!(csv(Some("ci-infra, deploys")), ["ci-infra", "deploys"]);
    }

    #[test]
    fn a_page_request_clamps_what_a_caller_asks_for() {
        let huge = page(None, Some(1_000_000)).unwrap();
        assert_eq!(huge.effective_limit(), PageRequest::MAX_LIMIT);

        let defaulted = page(None, None).unwrap();
        assert_eq!(defaulted.effective_limit(), PageRequest::DEFAULT_LIMIT);
    }

    #[test]
    fn a_cursor_that_is_not_ours_is_a_refusal_rather_than_a_silent_restart() {
        // Silently restarting from the beginning would replay the whole backlog to
        // a client that only mistyped.
        assert!(page(Some("not-a-cursor"), None).is_err());
    }
}
