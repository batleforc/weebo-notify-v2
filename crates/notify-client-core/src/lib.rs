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
mod deeplink;
mod dnd;
mod election;
mod error;
mod session;
mod signal;
mod store;
mod sync;
mod tray;

pub use config::{ConfigError, DEFAULT_PROFILE, Profile};
pub use deeplink::{DeepLink, SCHEME, parse_deep_link};
pub use dnd::{DndPreset, now_utc, quiet_now};
pub use election::{StreamLease, lock_path, try_hold_stream};
pub use error::ClientError;
pub use session::{Revision, Session};
pub use signal::{StreamSignal, StreamWatch, signal_path};
pub use store::{CacheEntry, CachedNotification, Store, store_path};
pub use sync::SyncOutcome;
pub use tray::{Link, Tray, TrayIcon, tray};

/// The value types a front end needs to name — re-exported so nothing above has to
/// depend on the generated crate directly.
pub mod types {
    pub use notify_client_api::types::{
        Action, ActionKey, Actions, Channel, ChannelId, ChannelPolicyBody, ChoiceOption,
        ChoiceOptionKey, Day, Device, Dnd, DndDraft, DndScope, HumanDuration, Image, Interaction,
        Notification, Principal, Priority, Progress, Published, QuietHours, Scope, SilenceBody,
        SilenceScope, State, Status, Ttl, UnreadCount, UserId, WallClock,
    };
}
