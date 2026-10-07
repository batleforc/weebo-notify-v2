//! Credential verification (design §5.1, §5.2): OIDC access tokens and API-key secrets. Stateless, no sessions.
//! Security: key algorithms come from the JWKS entry, never the token's `alg` (symmetric keys refused);
//! unknown `kid` refetches are throttled; the discovery document must name the configured issuer.

mod claims;
mod config;
mod federation;
mod guard;
mod jwks;
mod secrets;
#[cfg(test)]
mod test_support;
mod verifier;

pub use config::OidcConfig;
pub use federation::FederationTokens;
pub use guard::GuardedSecretHasher;
pub use jwks::{HttpJwksSource, JwksSource};
pub use secrets::Argon2SecretHasher;
pub use verifier::OidcVerifier;
