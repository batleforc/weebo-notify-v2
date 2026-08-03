//! Everything a front end needs that is not presentation (design §4, §9.3).
//!
//! The CLI, the TUI and the Tauri shell are three renderings of one client. What
//! they share lives here: which server to talk to, how to present a credential,
//! what a refusal means. The rule that keeps it that way is stated in the design —
//! any business logic that appears in `notify-cli` belongs down here, because Tauri
//! will want it too.
//!
//! # What is here in M1
//!
//! Configuration, an authenticated session, and the two ingestion calls
//! (`publish` and `update`). The local SQLite store, the sync cursor and the SSE
//! reconnection loop (design §9.3bis) land with the TUI in M2 — the shape above
//! them does not change when they do.
//!
//! # The generated client is a boundary, not an interface
//!
//! [`notify_client_api`] is regenerated from the committed OpenAPI document
//! (ADR-0009), so its types move when the API moves and its ergonomics are
//! progenitor's rather than ours. Front ends depend on *this* crate instead, so a
//! regeneration is a change here and not in three user interfaces.

mod config;
mod error;
mod session;

pub use config::{ConfigError, DEFAULT_PROFILE, Profile};
pub use error::ClientError;
pub use session::{Revision, Session};

/// The value types a front end needs to name — re-exported so nothing above has to
/// depend on the generated crate directly.
pub mod types {
    pub use notify_client_api::types::{
        Action, Actions, Image, Notification, Priority, Progress, Published, State, Status, Ttl,
    };
}
