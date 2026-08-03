//! The everyday identity provider for the development stack (design §12.1).
//!
//! Just enough OIDC to exercise the real thing and nothing more:
//! discovery, a JWKS, Authorization Code with PKCE, refresh, userinfo and logout.
//! Tokens are signed with a real ES256 key, so `notify-oidc` runs its production
//! verification path — this is a small provider, not a stub that hands out a
//! pre-baked string.
//!
//! Three refusals shape what is here:
//!
//! * **Never redirect anywhere the client did not register.** An unknown
//!   `redirect_uri` is a plain 400 in the browser, never a redirect — a mock that
//!   is relaxed about that teaches the habit production must not have.
//! * **Never accept a code without its verifier.** PKCE is mandatory: the flow the
//!   web and Tauri clients use is the only flow this provider knows.
//! * **Never invent a claim.** A user with no groups gets no `groups` claim at
//!   all, because that is what a provider without a groups mapping does, and it is
//!   the degradation path the server has to survive (T29).
//!
//! It exists because the third-party mock it replaces was not dependable enough to
//! sit on the critical path of every developer's morning — see
//! [ADR-0014](https://weebo-notify.docs/docs/adr/0014-a-first-party-oidc-mock).

mod config;
mod endpoints;
mod keys;
mod state;

pub use config::{Client, ConfigError, Identities, User};
pub use endpoints::configure;
pub use state::Mock;
