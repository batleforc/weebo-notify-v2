//! Everything a front end needs that is not presentation (design §4, §9.3).
//!
//! The CLI, the TUI and the Tauri shell are three renderings of one client. What
//! they share lives here: which server to talk to, how to present a credential,
//! what a refusal means. The rule that keeps it that way is stated in the design —
//! any business logic that appears in `notify-cli` belongs down here, because Tauri
//! will want it too.
//!
//! # What is here
//!
//! Configuration, an authenticated session and the ingestion calls (M1); the local
//! SQLite store, the sync cursor, the stream election and its local signal (M2,
//! design §9.3bis).
//!
//! # The generated client is a boundary, not an interface
//!
//! [`notify_client_api`] is regenerated from the committed OpenAPI document
//! (ADR-0009), so its types move when the API moves and its ergonomics are
//! progenitor's rather than ours. Front ends depend on *this* crate instead, so a
//! regeneration is a change here and not in three user interfaces.

mod config;
mod election;
mod error;
mod session;
mod signal;
mod store;
mod sync;

pub use config::{ConfigError, DEFAULT_PROFILE, Profile};
pub use election::{StreamLease, lock_path, try_hold_stream};
pub use error::ClientError;
pub use session::{Revision, Session};
pub use signal::{StreamSignal, StreamWatch, signal_path};
pub use store::{CacheEntry, CachedNotification, Store, store_path};
pub use sync::SyncOutcome;

/// The value types a front end needs to name — re-exported so nothing above has to
/// depend on the generated crate directly.
pub mod types {
    pub use notify_client_api::types::{
        Action, Actions, Channel, ChannelPolicyBody, Day, Device, Dnd, DndDraft, DndScope, Image,
        Notification, Principal, Priority, Progress, Published, QuietHours, Scope, State, Status,
        Ttl, WallClock,
    };
}
