//! What the mock remembers between requests (in memory only: a restart logs everyone out).

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError, RwLock};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::JwkSet;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use ulid::Ulid;

use crate::config::{Client, Identities, User};
use crate::keys::{KeyError, Keys};

/// How long a login page stays valid once `/authorize` has rendered it.
const REQUEST_TTL_SECS: i64 = 600;

/// How long an authorization code lives (RFC 6749 allows up to 10 minutes).
const CODE_TTL_SECS: i64 = 60;

/// A parsed, validated `/authorize` request waiting for someone to log in.
#[derive(Debug, Clone)]
pub(crate) struct AuthRequest {
    pub(crate) redirect_uri: String,
    pub(crate) state: Option<String>,
    pub(crate) nonce: Option<String>,
    pub(crate) challenge: String,
    pub(crate) scope: String,
    pub(crate) expires_at: i64,
}

/// Someone logged in, and this is what they are owed.
#[derive(Debug, Clone)]
pub(crate) struct Grant {
    pub(crate) sub: String,
    pub(crate) scope: String,
    pub(crate) nonce: Option<String>,
    pub(crate) redirect_uri: String,
    pub(crate) challenge: String,
    pub(crate) auth_time: i64,
    pub(crate) expires_at: i64,
}

/// One issued token set.
#[derive(Debug)]
pub(crate) struct Tokens {
    pub(crate) access_token: String,
    pub(crate) id_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) expires_in: i64,
    pub(crate) scope: String,
}

/// Why a token request was refused, mapped onto the OAuth error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrantRefusal {
    /// The code or refresh token is unknown, spent or expired.
    UnknownGrant,
    /// The `code_verifier` does not hash to the challenge that was registered.
    PkceMismatch,
    /// The `redirect_uri` is not the one the code was issued for.
    RedirectMismatch,
    /// The subject vanished from the identities file between the two calls.
    UnknownUser,
}

/// The provider.
#[derive(Debug)]
pub struct Mock {
    identities: Identities,
    keys: RwLock<Keys>,
    requests: Mutex<HashMap<String, AuthRequest>>,
    codes: Mutex<HashMap<String, Grant>>,
    refresh: Mutex<HashMap<String, Grant>>,
}

impl Mock {
    /// Build a provider over a set of identities.
    ///
    /// # Errors
    ///
    /// Fails only if the signing key cannot be derived (a bug in this crate).
    pub fn new(identities: Identities) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self {
            identities,
            keys: RwLock::new(Keys::new()?),
            requests: Mutex::new(HashMap::new()),
            codes: Mutex::new(HashMap::new()),
            refresh: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn issuer(&self) -> &str {
        &self.identities.issuer
    }

    pub(crate) fn client(&self) -> &Client {
        &self.identities.client
    }

    pub(crate) fn identities(&self) -> &Identities {
        &self.identities
    }

    /// The published key set.
    pub(crate) fn jwks(&self) -> JwkSet {
        self.keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .jwk_set()
    }

    /// Retire the signing key and mint a new one, returning the new key id.
    pub(crate) fn rotate_keys(&self) -> Result<String, KeyError> {
        self.keys
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .rotate()
            .map(ToOwned::to_owned)
    }

    /// Park a validated `/authorize` request and return the opaque handle the login form posts
    /// back (so the page never reflects attacker-controlled strings).
    pub(crate) fn begin(&self, request: AuthRequest) -> String {
        let id = Ulid::generate().to_string();
        let mut requests = self.requests.lock().unwrap_or_else(PoisonError::into_inner);

        prune(&mut requests, |request| request.expires_at);
        requests.insert(id.clone(), request);

        id
    }

    /// Look a parked request back up, leaving it in place so a mistyped password can be retried.
    pub(crate) fn pending(&self, id: &str) -> Option<AuthRequest> {
        let requests = self.requests.lock().unwrap_or_else(PoisonError::into_inner);

        requests
            .get(id)
            .filter(|request| request.expires_at > now())
            .cloned()
    }

    /// Turn a successful login into an authorization code, consuming the request.
    pub(crate) fn authorize(&self, id: &str, user: &User) -> Option<(String, AuthRequest)> {
        let request = self
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .filter(|request| request.expires_at > now())?;

        let code = Ulid::generate().to_string();
        let mut codes = self.codes.lock().unwrap_or_else(PoisonError::into_inner);

        prune(&mut codes, |grant| grant.expires_at);
        codes.insert(
            code.clone(),
            Grant {
                sub: user.sub.clone(),
                scope: request.scope.clone(),
                nonce: request.nonce.clone(),
                redirect_uri: request.redirect_uri.clone(),
                challenge: request.challenge.clone(),
                auth_time: now(),
                expires_at: now() + CODE_TTL_SECS,
            },
        );

        Some((code, request))
    }

    /// Exchange a code for tokens; the code is spent before any check, so a replay always fails.
    pub(crate) fn redeem_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<Tokens, GrantRefusal> {
        let grant = self
            .codes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(code)
            .filter(|grant| grant.expires_at > now())
            .ok_or(GrantRefusal::UnknownGrant)?;

        if grant.redirect_uri != redirect_uri {
            return Err(GrantRefusal::RedirectMismatch);
        }
        if challenge_for(verifier) != grant.challenge {
            return Err(GrantRefusal::PkceMismatch);
        }

        self.issue(&grant)
    }

    /// Exchange a refresh token for a fresh set, rotating it as Authentik does (§3.2 `oidc-compat`).
    pub(crate) fn redeem_refresh(&self, token: &str) -> Result<Tokens, GrantRefusal> {
        let grant = self
            .refresh
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(token)
            .filter(|grant| grant.expires_at > now())
            .ok_or(GrantRefusal::UnknownGrant)?;

        self.issue(&grant)
    }

    /// Mint the token set a grant is owed, plus a refresh token for offline access.
    fn issue(&self, grant: &Grant) -> Result<Tokens, GrantRefusal> {
        let user = self
            .identities
            .user(&grant.sub)
            .ok_or(GrantRefusal::UnknownUser)?;

        let keys = self.keys.read().unwrap_or_else(PoisonError::into_inner);
        let signer = keys.active();
        let client = &self.identities.client;
        let issued_at = now();

        let mut access = self.claims(user, grant, issued_at, client.access_token_ttl_secs);
        access["scope"] = json!(grant.scope);
        access["azp"] = json!(client.id);

        let mut id_token = self.claims(user, grant, issued_at, client.id_token_ttl_secs);
        if let Some(nonce) = &grant.nonce {
            // Binds the id token to the browser session that started the flow.
            id_token["nonce"] = json!(nonce);
        }

        let access_token = signer.sign(&access).map_err(|error| {
            tracing::error!(%error, "the mock could not sign an access token");
            GrantRefusal::UnknownGrant
        })?;
        let id_token = signer.sign(&id_token).map_err(|error| {
            tracing::error!(%error, "the mock could not sign an id token");
            GrantRefusal::UnknownGrant
        })?;

        let refresh_token = grant
            .scope
            .split_whitespace()
            .any(|scope| scope == "offline_access")
            .then(|| {
                let token = Ulid::generate().to_string();
                let mut refresh = self.refresh.lock().unwrap_or_else(PoisonError::into_inner);

                prune(&mut refresh, |grant| grant.expires_at);
                refresh.insert(
                    token.clone(),
                    Grant {
                        expires_at: issued_at + client.refresh_token_ttl_secs,
                        ..grant.clone()
                    },
                );

                token
            });

        Ok(Tokens {
            access_token,
            id_token,
            refresh_token,
            expires_in: client.access_token_ttl_secs,
            scope: grant.scope.clone(),
        })
    }

    /// The claims both tokens share.
    fn claims(&self, user: &User, grant: &Grant, issued_at: i64, ttl: i64) -> serde_json::Value {
        let mut claims = json!({
            "iss": self.identities.issuer,
            "sub": user.sub,
            "aud": self.identities.client.id,
            "iat": issued_at,
            "nbf": issued_at,
            "exp": issued_at + ttl,
            "auth_time": grant.auth_time,
            "jti": Ulid::generate().to_string(),
            "preferred_username": user.username,
            "name": user.name,
            "email": user.email,
            "email_verified": user.email_verified,
        });

        // Absent, not empty: the server must survive a provider with no groups mapping (T29).
        if !user.groups.is_empty() {
            claims["groups"] = json!(user.groups);
        }

        claims
    }

    /// Verify one of our own access tokens, for `/userinfo`.
    pub(crate) fn verify(&self, token: &str) -> Option<serde_json::Value> {
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
        validation.set_issuer(&[&self.identities.issuer]);
        validation.set_audience(&[&self.identities.client.id]);

        let keys = self.keys.read().unwrap_or_else(PoisonError::into_inner);

        // Every published key, so a token minted before a rotation still resolves.
        keys.signers().find_map(|signer| {
            jsonwebtoken::decode::<serde_json::Value>(token, signer.decoding(), &validation)
                .ok()
                .map(|data| data.claims)
        })
    }
}

/// When a login page rendered now stops being usable.
pub(crate) fn request_deadline() -> i64 {
    now() + REQUEST_TTL_SECS
}

/// The S256 challenge a verifier produces (RFC 7636 §4.2).
pub(crate) fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Drop everything that has expired (on insert, no background task needed for a dev provider).
fn prune<T>(entries: &mut HashMap<String, T>, expiry: impl Fn(&T) -> i64) {
    let cutoff = now();
    entries.retain(|_, entry| expiry(entry) > cutoff);
}

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    pub(crate) fn identities() -> Identities {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../dev/oidc-mock/identities.toml");

        Identities::load(&path).expect("the committed identities file")
    }

    fn mock() -> Mock {
        Mock::new(identities()).expect("a provider builds")
    }

    fn request(challenge: &str) -> AuthRequest {
        AuthRequest {
            redirect_uri: "http://localhost:5173/auth/callback".to_owned(),
            state: Some("opaque".to_owned()),
            nonce: Some("once".to_owned()),
            challenge: challenge.to_owned(),
            scope: "openid profile email groups offline_access".to_owned(),
            expires_at: now() + REQUEST_TTL_SECS,
        }
    }

    /// Walk a whole flow and return the tokens.
    fn login(mock: &Mock, username: &str, verifier: &str) -> Tokens {
        let id = mock.begin(request(&challenge_for(verifier)));
        let user = mock
            .identities()
            .authenticate(username, "weebo-dev")
            .expect("a known account")
            .clone();
        let (code, parked) = mock.authorize(&id, &user).expect("the login is accepted");

        mock.redeem_code(&code, verifier, &parked.redirect_uri)
            .expect("the code is redeemable")
    }

    #[test]
    fn a_full_flow_yields_tokens_that_carry_the_users_groups() {
        let mock = mock();
        let tokens = login(&mock, "alice", "a-verifier-long-enough-to-be-plausible");

        let claims = mock.verify(&tokens.access_token).expect("our own token");

        assert_eq!(claims["sub"], "alice");
        assert_eq!(claims["groups"], json!(["infra-admins", "platform-admins"]));
        assert_eq!(claims["aud"], "weebo-notify");
        assert_eq!(claims["iss"], mock.issuer());
    }

    #[test]
    fn a_user_without_groups_gets_no_groups_claim_rather_than_an_empty_one() {
        let mock = mock();
        let tokens = login(&mock, "carol", "another-perfectly-fine-verifier");

        let claims = mock.verify(&tokens.access_token).unwrap();

        assert!(
            claims.get("groups").is_none(),
            "an empty array would hide the degradation path T29 exists to test"
        );
    }

    #[test]
    fn a_code_cannot_be_redeemed_twice() {
        let mock = mock();
        let verifier = "a-verifier-long-enough-to-be-plausible";
        let id = mock.begin(request(&challenge_for(verifier)));
        let user = mock.identities().user("bob").unwrap().clone();
        let (code, parked) = mock.authorize(&id, &user).unwrap();

        assert!(
            mock.redeem_code(&code, verifier, &parked.redirect_uri)
                .is_ok()
        );
        assert_eq!(
            mock.redeem_code(&code, verifier, &parked.redirect_uri)
                .err(),
            Some(GrantRefusal::UnknownGrant)
        );
    }

    #[test]
    fn a_code_is_worthless_without_the_verifier_that_produced_its_challenge() {
        let mock = mock();
        let id = mock.begin(request(&challenge_for("the-real-verifier")));
        let user = mock.identities().user("alice").unwrap().clone();
        let (code, parked) = mock.authorize(&id, &user).unwrap();

        assert_eq!(
            mock.redeem_code(&code, "a-stolen-code-is-not-enough", &parked.redirect_uri)
                .err(),
            Some(GrantRefusal::PkceMismatch)
        );
    }

    #[test]
    fn redeeming_a_code_against_a_different_redirect_uri_is_refused() {
        let mock = mock();
        let verifier = "a-verifier-long-enough-to-be-plausible";
        let id = mock.begin(request(&challenge_for(verifier)));
        let user = mock.identities().user("alice").unwrap().clone();
        let (code, _) = mock.authorize(&id, &user).unwrap();

        assert_eq!(
            mock.redeem_code(&code, verifier, "http://localhost:8080/auth/callback")
                .err(),
            Some(GrantRefusal::RedirectMismatch)
        );
    }

    #[test]
    fn a_refresh_token_is_spent_by_the_refresh_it_pays_for() {
        let mock = mock();
        let tokens = login(&mock, "alice", "a-verifier-long-enough-to-be-plausible");
        let first = tokens.refresh_token.expect("offline_access was requested");

        let refreshed = mock.redeem_refresh(&first).expect("the first use works");
        let second = refreshed.refresh_token.expect("rotation issues a new one");

        assert_ne!(first, second, "Authentik rotates; so does the mock");
        assert_eq!(
            mock.redeem_refresh(&first).err(),
            Some(GrantRefusal::UnknownGrant),
            "reusing a rotated refresh token must fail, or clients never learn to \
             store the new one"
        );
    }

    #[test]
    fn a_token_signed_before_a_rotation_still_verifies_after_it() {
        let mock = mock();
        let tokens = login(&mock, "alice", "a-verifier-long-enough-to-be-plausible");

        let fresh_kid = mock.rotate_keys().unwrap();

        assert!(
            mock.verify(&tokens.access_token).is_some(),
            "retiring a key must not log out everyone holding a live token"
        );
        assert!(
            mock.jwks()
                .keys
                .iter()
                .any(|jwk| jwk.common.key_id.as_deref() == Some(fresh_kid.as_str()))
        );
    }

    #[test]
    fn a_login_handle_is_consumed_by_the_login_it_completes() {
        let mock = mock();
        let id = mock.begin(request(&challenge_for("verifier")));
        let user = mock.identities().user("alice").unwrap().clone();

        assert!(mock.authorize(&id, &user).is_some());
        assert!(
            mock.authorize(&id, &user).is_none(),
            "one login page is one grant"
        );
    }
}
