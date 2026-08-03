//! Credential verification (design §5.1, §5.2).
//!
//! Two things arrive on a request and both are checked here, behind ports the
//! application layer already knows: an OIDC access token ([`OidcVerifier`]) and the
//! secret half of an API key ([`Argon2SecretHasher`]). They share a crate because
//! they answer the same question — "is this credential genuine" — and because
//! neither has any business knowing what the caller then goes on to do.
//!
//! # No sessions, anywhere
//!
//! A user token is validated against the issuer's JWKS and nothing is written down
//! (design §5.1). That is what keeps horizontal scaling trivial: a replica that has
//! never seen a caller can serve their first request, and an IdP outage degrades to
//! "nobody can log in fresh" rather than "nobody can use the product".
//!
//! # What the verifier refuses to do
//!
//! **Trust the token's own `alg`.** The algorithms accepted for a key come from the
//! *JWKS entry*, never from the header. Taking the header's word for it is the
//! algorithm-confusion family of attacks — most famously handing an RSA public key
//! to an HMAC verifier as if it were a shared secret. Symmetric keys in a JWKS are
//! rejected outright for the same reason.
//!
//! **Refetch on demand.** An unknown `kid` triggers at most one refresh per
//! [`OidcConfig::min_refresh_interval`]. Without that, anybody can turn a stream of
//! forged tokens into a stream of requests to the IdP — a denial of service aimed at
//! somebody else's infrastructure, paid for by ours.
//!
//! **Guess the issuer.** The discovery document has to name the issuer we
//! configured. A document that names a different one is not a source of keys, it is
//! a redirect somewhere we did not intend to go.

mod claims;
mod config;
mod jwks;
mod secrets;
#[cfg(test)]
mod test_support;
mod verifier;

pub use config::OidcConfig;
pub use jwks::{HttpJwksSource, JwksSource};
pub use secrets::Argon2SecretHasher;
pub use verifier::OidcVerifier;
