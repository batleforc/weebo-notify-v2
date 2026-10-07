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

/// An authenticated caller, extracted from `Authorization`; there is no anonymous path.
#[derive(Debug, Clone)]
pub struct Authenticated(pub Caller);

impl FromRequest for Authenticated {
    type Error = ApiError;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let presented = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        authenticate(req, presented)
    }
}

/// Verify an `Authorization`-shaped value; shared with the compat routes' extractor.
pub(crate) fn authenticate(
    req: &HttpRequest,
    presented: Option<String>,
) -> Pin<Box<dyn Future<Output = Result<Authenticated, ApiError>>>> {
    let state = req.app_data::<web::Data<ApiState>>().cloned();
    Box::pin(async move {
        // Missing app data is a wiring bug, not the caller's fault.
        let Some(state) = state else {
            return Err(PortError::NotConfigured { what: "http.state" }.into());
        };
        let credential = credential_from(presented.as_deref())?;
        Ok(Authenticated(state.authenticate.execute(credential).await?))
    })
}

/// Read the `Authorization` header: keys and tokens both arrive as `Bearer` (design §8),
/// told apart by our minted `wnk_` prefix so a JWT is never mistaken for a key.
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

/// Parse a string-shaped domain enum through serde, so a query accepts exactly what
/// the JSON body accepts.
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

/// Split a comma-separated parameter, dropping empties (`?channels=` means no filter).
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
        // Restarting silently would replay the whole backlog to a client that mistyped.
        assert!(page(Some("not-a-cursor"), None).is_err());
    }
}
