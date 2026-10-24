//! From a verified payload to the handful of claims authorisation acts on.

use notify_ports::{TokenRejection, VerifiedClaims};
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use url::Url;

/// The payload as it arrives; non-standard claims stay flat for the configurable groups lookup.
#[derive(Debug, Deserialize)]
pub(crate) struct RawClaims {
    iss: String,
    sub: String,
    exp: i64,
    /// Checked by the verifier against its injected clock, like `exp`.
    pub(crate) nbf: Option<i64>,
    email: Option<String>,
    /// Some issuers send it as the string `"true"`; anything else, absent included,
    /// is unverified.
    #[serde(default)]
    email_verified: Option<Value>,
    name: Option<String>,
    preferred_username: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

impl RawClaims {
    /// Reduce to what the application layer may see; everything else is dropped so nothing depends on it.
    pub(crate) fn into_verified(
        self,
        groups_claim: &str,
        trust_unverified_email: bool,
    ) -> Result<VerifiedClaims, TokenRejection> {
        let issuer = Url::parse(&self.iss).map_err(|_| TokenRejection::Malformed)?;
        let expires_at =
            OffsetDateTime::from_unix_timestamp(self.exp).map_err(|_| TokenRejection::Malformed)?;

        if self.sub.is_empty() {
            return Err(TokenRejection::Malformed);
        }

        let verified = match &self.email_verified {
            Some(Value::Bool(flag)) => *flag,
            Some(Value::String(flag)) => flag == "true",
            _ => false,
        };

        let groups = self
            .extra
            .get(groups_claim)
            .map(normalise_groups)
            .unwrap_or_default();

        let trusted = trust_unverified_email || verified;
        Ok(VerifiedClaims {
            issuer,
            subject: self.sub,
            email_unverified: self.email.is_some() && !trusted,
            // An unverified address must not reach the users table: `email` bindings match on it.
            email: self.email.filter(|_| trusted),
            // Fall back to `preferred_username`; neither is required.
            display_name: self.name.or(self.preferred_username),
            groups,
            expires_at,
        })
    }
}

/// Read a groups claim as an array, a single string, or a space-separated string.
/// Anything else is ignored rather than failing the login.
fn normalise_groups(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str())
            .filter(|group| !group.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Value::String(single) => single.split_whitespace().map(ToOwned::to_owned).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn claims(payload: serde_json::Value) -> RawClaims {
        serde_json::from_value(payload).unwrap()
    }

    #[test]
    fn the_claims_we_act_on_survive_and_the_rest_does_not() {
        let verified = claims(json!({
            "iss": "https://idp.example",
            "sub": "alice",
            "exp": 1_800_000_000_i64,
            "email": "alice@weebo.local",
            "email_verified": true,
            "name": "Alice Martin",
            "groups": ["infra-admins", "platform-admins"],
            "acr": "urn:something we do not read",
        }))
        .into_verified("groups", false)
        .unwrap();

        assert_eq!(verified.subject, "alice");
        assert_eq!(verified.email.as_deref(), Some("alice@weebo.local"));
        assert_eq!(verified.display_name.as_deref(), Some("Alice Martin"));
        assert_eq!(verified.groups, ["infra-admins", "platform-admins"]);
    }

    #[test]
    fn an_unverified_email_is_dropped_unless_the_operator_trusts_it() {
        let payload = |verified: serde_json::Value| {
            claims(json!({
                "iss": "https://idp.example",
                "sub": "mallory",
                "exp": 1_800_000_000_i64,
                "email": "alice@weebo.local",
                "email_verified": verified,
            }))
        };

        for unverified in [json!(false), json!("false"), serde_json::Value::Null] {
            let claims = payload(unverified).into_verified("groups", false).unwrap();
            assert!(
                claims.email.is_none(),
                "an address nobody vouched for must not reach an email binding"
            );
            // ...and is reported, so the stored one is cleared rather than kept.
            assert!(claims.email_unverified);
        }
        let absent = claims(json!({
            "iss": "https://idp.example",
            "sub": "mallory",
            "exp": 1_800_000_000_i64,
        }))
        .into_verified("groups", false)
        .unwrap();
        assert!(
            !absent.email_unverified,
            "no claim at all is no news, and keeps the stored address"
        );
        assert!(
            payload(json!("true"))
                .into_verified("groups", false)
                .unwrap()
                .email
                .is_some()
        );
        assert!(
            payload(json!(false))
                .into_verified("groups", true)
                .unwrap()
                .email
                .is_some()
        );
    }

    #[test]
    fn an_issuer_that_emits_no_groups_yields_no_groups_rather_than_a_failure() {
        // Dex (T29): login works and reaches only individual bindings.
        let verified = claims(json!({
            "iss": "https://idp.example",
            "sub": "carol",
            "exp": 1_800_000_000_i64,
        }))
        .into_verified("groups", false)
        .unwrap();

        assert_eq!(verified.groups, [] as [std::string::String; 0]);
        assert!(verified.email.is_none());
        assert!(verified.display_name.is_none());
    }

    #[test]
    fn the_groups_claim_is_read_wherever_the_operator_put_it() {
        let verified = claims(json!({
            "iss": "https://idp.example",
            "sub": "bob",
            "exp": 1_800_000_000_i64,
            "groups": ["ignored"],
            "weebo/roles": ["dev-team"],
        }))
        .into_verified("weebo/roles", false)
        .unwrap();

        assert_eq!(verified.groups, ["dev-team"]);
    }

    #[test]
    fn preferred_username_stands_in_when_there_is_no_name() {
        let verified = claims(json!({
            "iss": "https://idp.example",
            "sub": "bob",
            "exp": 1_800_000_000_i64,
            "preferred_username": "bob",
        }))
        .into_verified("groups", false)
        .unwrap();

        assert_eq!(verified.display_name.as_deref(), Some("bob"));
    }

    #[test]
    fn every_shape_of_groups_claim_an_idp_emits_is_read() {
        let single = normalise_groups(&json!("dev-team"));
        assert_eq!(single, ["dev-team"]);

        let spaced = normalise_groups(&json!("dev-team infra-admins"));
        assert_eq!(spaced, ["dev-team", "infra-admins"]);

        let array = normalise_groups(&json!(["dev-team", "", "infra-admins"]));
        assert_eq!(
            array,
            ["dev-team", "infra-admins"],
            "empties are not groups"
        );
    }

    #[test]
    fn a_groups_claim_we_cannot_read_costs_the_groups_not_the_login() {
        assert_eq!(normalise_groups(&json!(42)), [] as [std::string::String; 0]);
        assert_eq!(
            normalise_groups(&json!({ "nested": "object" })),
            [] as [std::string::String; 0]
        );
        assert_eq!(
            normalise_groups(&json!(null)),
            [] as [std::string::String; 0]
        );
    }

    #[test]
    fn an_unparseable_issuer_or_expiry_is_malformed() {
        let bad_issuer = claims(json!({ "iss": "not a url", "sub": "a", "exp": 1_i64 }))
            .into_verified("groups", false)
            .unwrap_err();
        assert_eq!(bad_issuer, TokenRejection::Malformed);

        let bad_exp = claims(json!({ "iss": "https://x.example", "sub": "a", "exp": i64::MAX }))
            .into_verified("groups", false)
            .unwrap_err();
        assert_eq!(bad_exp, TokenRejection::Malformed);

        let no_subject = claims(json!({ "iss": "https://x.example", "sub": "", "exp": 1_i64 }))
            .into_verified("groups", false)
            .unwrap_err();
        assert_eq!(no_subject, TokenRejection::Malformed);
    }
}

/// A CI job's ID token as it arrives (design §5.3); separate from [`RawClaims`] because the rules differ.
#[derive(Debug, Deserialize)]
pub(crate) struct RawFederatedClaims {
    iss: String,
    sub: String,
    exp: i64,
    /// Checked by the verifier against its injected clock, like `exp`.
    pub(crate) nbf: Option<i64>,
    /// One string or several; RFC 7519 allows both spellings.
    aud: Audience,
    jti: Option<String>,
}

/// `aud` is a string or an array (RFC 7519).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum Audience {
    One(String),
    Many(Vec<String>),
}

impl RawFederatedClaims {
    /// Reduce to what the domain decides on.
    pub(crate) fn into_federated(self) -> Result<notify_domain::FederatedClaims, TokenRejection> {
        let expires_at =
            OffsetDateTime::from_unix_timestamp(self.exp).map_err(|_| TokenRejection::Malformed)?;
        if self.sub.is_empty() || self.iss.is_empty() {
            return Err(TokenRejection::Malformed);
        }

        Ok(notify_domain::FederatedClaims {
            issuer: self.iss,
            subject: self.sub,
            audiences: match self.aud {
                Audience::One(one) => vec![one],
                Audience::Many(many) => many,
            },
            // Kept even when absent: refusing a missing id is the domain's rule (T11).
            jti: self.jti,
            // The bare `exp`; the verifier adds its own leeway once it has checked it.
            accepted_until: expires_at,
        })
    }
}
