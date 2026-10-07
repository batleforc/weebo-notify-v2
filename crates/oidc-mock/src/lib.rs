//! The development identity provider (§12.1, ADR-0014): discovery, JWKS, auth code + PKCE,
//! refresh, userinfo and logout, signed with real ES256. It never redirects to an unregistered
//! URI, never accepts a code without its verifier, and never invents a claim (T29).

mod config;
mod endpoints;
mod keys;
mod state;

pub use config::{Client, ConfigError, Identities, User};
pub use endpoints::configure;
pub use state::Mock;
