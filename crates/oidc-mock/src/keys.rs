//! The signing keys the mock publishes.
//!
//! Real ES256, not a shortcut: the server verifies these tokens through the same
//! `notify-oidc` path it uses against Authentik, so a mistake in that path fails in
//! development rather than in staging.
//!
//! Each scalar is derived from its key id rather than being random or committed. A
//! private key in the repository is a gitleaks finding waiting to happen, and a
//! random one would give a different JWKS on every restart — which would turn a
//! browser refresh into a logout. These keys protect nothing.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header};
use p256::SecretKey;
use p256::elliptic_curve::sec1::ToSec1Point as _;
use p256::pkcs8::{EncodePrivateKey as _, LineEnding};

/// Something went wrong with a key or a signature. Every variant is a bug in the
/// mock, never a bad request.
#[derive(Debug, thiserror::Error)]
pub(crate) enum KeyError {
    /// The derived scalar or its encoding was rejected.
    #[error("could not build the signing key `{kid}`: {reason}")]
    Unbuildable {
        /// Which key.
        kid: String,
        /// What the cryptography said.
        reason: String,
    },

    /// Signing failed.
    #[error("could not sign a token: {0}")]
    Unsignable(String),
}

/// One key: it signs, and it publishes itself.
pub(crate) struct Signer {
    kid: String,
    encoding: EncodingKey,
    decoding: DecodingKey,
    jwk: Jwk,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The keys themselves are not printable and would be noise anyway.
        f.debug_struct("Signer")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// Build the key belonging to `kid`. Same id, same key, every run.
    pub(crate) fn derive(kid: &str) -> Result<Self, KeyError> {
        let fail = |reason: &dyn std::fmt::Display| KeyError::Unbuildable {
            kid: kid.to_owned(),
            reason: reason.to_string(),
        };

        let mut scalar = [0x11_u8; 32];
        // ASCII stays far below the curve order, so every id yields a valid scalar.
        for (slot, byte) in scalar.iter_mut().zip(kid.as_bytes()) {
            *slot = *byte;
        }

        let secret = SecretKey::from_slice(&scalar).map_err(|error| fail(&error))?;
        let pem = secret
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|error| fail(&error))?;
        let encoding = EncodingKey::from_ec_pem(pem.as_bytes()).map_err(|error| fail(&error))?;

        let point = secret.public_key().to_sec1_point(false);
        let (Some(x), Some(y)) = (point.x(), point.y()) else {
            return Err(fail(&"an uncompressed point has both coordinates"));
        };
        let (x, y) = (URL_SAFE_NO_PAD.encode(x), URL_SAFE_NO_PAD.encode(y));

        let published = serde_json::json!({
            "kty": "EC",
            "crv": "P-256",
            "x": x,
            "y": y,
            "kid": kid,
            "alg": "ES256",
            "use": "sig",
        });
        let jwk: Jwk = serde_json::from_value(published).map_err(|error| fail(&error))?;
        let decoding = DecodingKey::from_ec_components(&x, &y).map_err(|error| fail(&error))?;

        Ok(Self {
            kid: kid.to_owned(),
            encoding,
            decoding,
            jwk,
        })
    }

    /// Sign a claim set, naming the key in the header so the server can pick it out
    /// of the JWKS — which is the whole reason rotation works.
    pub(crate) fn sign(&self, claims: &serde_json::Value) -> Result<String, KeyError> {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.kid.clone());

        jsonwebtoken::encode(&header, claims, &self.encoding)
            .map_err(|error| KeyError::Unsignable(error.to_string()))
    }

    pub(crate) fn kid(&self) -> &str {
        &self.kid
    }

    pub(crate) fn decoding(&self) -> &DecodingKey {
        &self.decoding
    }
}

/// The active key plus every key it replaced.
///
/// Retired keys stay published on purpose: an access token minted a minute before a
/// rotation must keep verifying until it expires. A provider that drops the old key
/// immediately logs everyone out, and the E2E suite would be testing that instead of
/// rotation.
#[derive(Debug)]
pub(crate) struct Keys {
    active: Signer,
    retired: Vec<Signer>,
    generation: u32,
}

/// Where the derived key ids come from — `weebo-dev-1`, `weebo-dev-2`, …
const KID_PREFIX: &str = "weebo-dev-";

impl Keys {
    /// The key set a fresh mock starts with.
    pub(crate) fn new() -> Result<Self, KeyError> {
        Ok(Self {
            active: Signer::derive(&format!("{KID_PREFIX}1"))?,
            retired: Vec::new(),
            generation: 1,
        })
    }

    pub(crate) fn active(&self) -> &Signer {
        &self.active
    }

    /// Every key, newest first — what `/jwks.json` publishes.
    pub(crate) fn jwk_set(&self) -> JwkSet {
        JwkSet {
            keys: std::iter::once(&self.active)
                .chain(&self.retired)
                .map(|signer| signer.jwk.clone())
                .collect(),
        }
    }

    /// Every key that could have signed a token, for verifying our own.
    pub(crate) fn signers(&self) -> impl Iterator<Item = &Signer> {
        std::iter::once(&self.active).chain(&self.retired)
    }

    /// Mint a new signing key and retire the current one.
    ///
    /// Exposed over HTTP so the auth suite can test key rotation on demand instead
    /// of waiting for a provider to decide to rotate (T29).
    pub(crate) fn rotate(&mut self) -> Result<&str, KeyError> {
        self.generation += 1;
        let fresh = Signer::derive(&format!("{KID_PREFIX}{}", self.generation))?;
        self.retired
            .push(std::mem::replace(&mut self.active, fresh));

        Ok(self.active.kid())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    #[test]
    fn the_same_id_always_derives_the_same_key() {
        // Otherwise every restart of the mock invalidates the tokens the browser is
        // holding, and a page refresh becomes a logout.
        let first = Signer::derive("weebo-dev-1").unwrap();
        let second = Signer::derive("weebo-dev-1").unwrap();

        assert_eq!(
            serde_json::to_value(&first.jwk).unwrap(),
            serde_json::to_value(&second.jwk).unwrap()
        );
    }

    #[test]
    fn a_signed_token_verifies_against_the_published_key() {
        let signer = Signer::derive("weebo-dev-1").unwrap();
        let token = signer
            .sign(&serde_json::json!({ "sub": "alice", "exp": 4_102_444_800_i64 }))
            .unwrap();

        let mut validation = jsonwebtoken::Validation::new(Algorithm::ES256);
        validation.validate_aud = false;

        let decoded =
            jsonwebtoken::decode::<serde_json::Value>(&token, signer.decoding(), &validation)
                .expect("the mock's own token verifies with the key it publishes");

        assert_eq!(decoded.claims["sub"], "alice");
    }

    #[test]
    fn rotation_publishes_the_new_key_without_dropping_the_old_one() {
        let mut keys = Keys::new().unwrap();
        let before = keys.active().kid().to_owned();

        let after = keys.rotate().unwrap().to_owned();

        assert_ne!(before, after);
        let published: Vec<_> = keys
            .jwk_set()
            .keys
            .iter()
            .filter_map(|jwk| jwk.common.key_id.clone())
            .collect();
        assert_eq!(
            published,
            [after, before],
            "a token minted just before the rotation must still verify"
        );
    }
}
