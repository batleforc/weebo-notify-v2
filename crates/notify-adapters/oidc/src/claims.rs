//! From a verified payload to the handful of claims authorisation acts on.

use notify_ports::{TokenRejection, VerifiedClaims};
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use url::Url;

/// The payload as it arrives.
///
/// Only the claims that are standard enough to name are named; the rest is kept
/// flat so the configurable groups claim can be looked up by whatever the operator
/// called it.
#[derive(Debug, Deserialize)]
pub(crate) struct RawClaims {
    iss: String,
    sub: String,
    exp: i64,
    email: Option<String>,
    name: Option<String>,
    preferred_username: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

impl RawClaims {
    /// Reduce to what the application layer is allowed to see.
    ///
    /// Anything not on [`VerifiedClaims`] is dropped here rather than carried
    /// along: a claim that never leaves this function is a claim no downstream code
    /// can quietly start depending on, which matters when the next IdP does not
    /// send it.
    pub(crate) fn into_verified(
        self,
        groups_claim: &str,
    ) -> Result<VerifiedClaims, TokenRejection> {
        let issuer = Url::parse(&self.iss).map_err(|_| TokenRejection::Malformed)?;
        let expires_at =
            OffsetDateTime::from_unix_timestamp(self.exp).map_err(|_| TokenRejection::Malformed)?;

        if self.sub.is_empty() {
            return Err(TokenRejection::Malformed);
        }

        let groups = self
            .extra
            .get(groups_claim)
            .map(normalise_groups)
            .unwrap_or_default();

        Ok(VerifiedClaims {
            issuer,
            subject: self.sub,
            email: self.email,
            // `name` is the OIDC claim; `preferred_username` is what an IdP with no
            // display name configured tends to send instead. Neither is required —
            // a user with no readable name is a user, not an error.
            display_name: self.name.or(self.preferred_username),
            groups,
            expires_at,
        })
    }
}

/// Read a groups claim in each of the three shapes issuers actually emit.
///
/// An array of strings is the common one. A single string happens when a user is in
/// exactly one group and the IdP does not bother with an array. A space-separated
/// string is the `scope`-style encoding some mappers produce. Anything else — a
/// number, an object, a nested array — is ignored rather than rejected: a group we
/// cannot read is a group the user does not get, and failing the whole login over
/// it would lock somebody out of channels they *are* individually bound to.
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
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

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
            "name": "Alice Martin",
            "groups": ["infra-admins", "platform-admins"],
            "acr": "urn:something we do not read",
        }))
        .into_verified("groups")
        .unwrap();

        assert_eq!(verified.subject, "alice");
        assert_eq!(verified.email.as_deref(), Some("alice@weebo.local"));
        assert_eq!(verified.display_name.as_deref(), Some("Alice Martin"));
        assert_eq!(verified.groups, ["infra-admins", "platform-admins"]);
    }

    #[test]
    fn an_issuer_that_emits_no_groups_yields_no_groups_rather_than_a_failure() {
        // Carol on Dex (scenario T29): the login has to work and reach exactly the
        // individual bindings, never a default grant.
        let verified = claims(json!({
            "iss": "https://idp.example",
            "sub": "carol",
            "exp": 1_800_000_000_i64,
        }))
        .into_verified("groups")
        .unwrap();

        assert!(verified.groups.is_empty());
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
        .into_verified("weebo/roles")
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
        .into_verified("groups")
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
        assert!(normalise_groups(&json!(42)).is_empty());
        assert!(normalise_groups(&json!({ "nested": "object" })).is_empty());
        assert!(normalise_groups(&json!(null)).is_empty());
    }

    #[test]
    fn an_unparseable_issuer_or_expiry_is_malformed() {
        let bad_issuer = claims(json!({ "iss": "not a url", "sub": "a", "exp": 1_i64 }))
            .into_verified("groups")
            .unwrap_err();
        assert_eq!(bad_issuer, TokenRejection::Malformed);

        let bad_exp = claims(json!({ "iss": "https://x.example", "sub": "a", "exp": i64::MAX }))
            .into_verified("groups")
            .unwrap_err();
        assert_eq!(bad_exp, TokenRejection::Malformed);

        let no_subject = claims(json!({ "iss": "https://x.example", "sub": "", "exp": 1_i64 }))
            .into_verified("groups")
            .unwrap_err();
        assert_eq!(no_subject, TokenRejection::Malformed);
    }
}
