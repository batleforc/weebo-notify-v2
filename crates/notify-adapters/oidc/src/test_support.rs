//! A real ES256 keypair for the tests, so the production verification path is the
//! one under test rather than a mock of it.
//!
//! The scalar is derived from the key id instead of being random or committed: a
//! private key in the repository is a gitleaks finding waiting to happen, and a
//! random one would make a failing test unreproducible. These keys protect nothing.

#![allow(
    clippy::expect_used,
    reason = "a fixture that cannot build its own key has no test to run"
)]

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use p256::SecretKey;
use p256::elliptic_curve::sec1::ToSec1Point as _;
use p256::pkcs8::{EncodePrivateKey as _, LineEnding};

/// A key that can sign test tokens and publish itself as a JWK.
pub(crate) struct Signer {
    kid: String,
    encoding: EncodingKey,
    jwk: Jwk,
}

impl Signer {
    /// Build the key belonging to `kid`. Same id, same key, every run.
    pub(crate) fn generate(kid: &str) -> Self {
        let mut scalar = [0x11_u8; 32];
        // ASCII stays far below the curve order, so every id yields a valid scalar.
        for (slot, byte) in scalar.iter_mut().zip(kid.as_bytes()) {
            *slot = *byte;
        }

        let secret =
            SecretKey::from_slice(&scalar).expect("a small non-zero scalar is a valid key");
        let pem = secret
            .to_pkcs8_pem(LineEnding::LF)
            .expect("a P-256 key always encodes");
        let encoding =
            EncodingKey::from_ec_pem(pem.as_bytes()).expect("pkcs8 is what from_ec_pem wants");

        let point = secret.public_key().to_sec1_point(false);
        let published = serde_json::json!({
            "kty": "EC",
            "crv": "P-256",
            "x": URL_SAFE_NO_PAD.encode(point.x().expect("an uncompressed point has an x")),
            "y": URL_SAFE_NO_PAD.encode(point.y().expect("an uncompressed point has a y")),
            "kid": kid,
            "alg": "ES256",
            "use": "sig",
        });
        let jwk: Jwk = serde_json::from_value(published).expect("an EC public jwk parses");

        Self {
            kid: kid.to_owned(),
            encoding,
            jwk,
        }
    }

    /// Sign an arbitrary claim set.
    pub(crate) fn sign(&self, claims: &serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, claims, &self.encoding).expect("signing a test token")
    }

    fn published(&self) -> Jwk {
        self.jwk.clone()
    }
}

/// The key set an issuer would publish for these signers.
pub(crate) fn jwk_set(signers: &[&Signer]) -> JwkSet {
    JwkSet {
        keys: signers.iter().map(|signer| signer.published()).collect(),
    }
}
