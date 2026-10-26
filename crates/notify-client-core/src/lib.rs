//! Everything a front end (CLI, TUI, Tauri) needs that is not presentation (§4, §9.3):
//! config, session, local store, sync, stream election. Front ends depend on this, not on
//! the generated client, so a regeneration changes one crate rather than three UIs.

mod config;
mod deeplink;
mod dnd;
mod election;
mod error;
pub mod instance;
mod session;
mod signal;
mod store;
mod sync;
mod tray;

pub use config::{ConfigError, DEFAULT_PROFILE, Profile, write_private};
pub use deeplink::{DeepLink, SCHEME, parse_deep_link};
pub use dnd::{DndPreset, now_utc, quiet_now};
pub use election::{StreamLease, lock_path, try_hold_stream};
pub use error::ClientError;
pub use session::{Revision, Session};
pub use signal::{StreamSignal, StreamWatch, signal_path};
pub use store::{CacheEntry, CachedNotification, Store, store_path, try_store_path};
pub use sync::SyncOutcome;
pub use tray::{Link, Tray, TrayIcon, tray};

/// The value types a front end needs to name, so nothing above depends on the generated crate.
pub mod types {
    pub use notify_client_api::types::{
        Action, ActionKey, Actions, Channel, ChannelId, ChannelPolicyBody, ChoiceOption,
        ChoiceOptionKey, CloseAfter, Day, Device, Dnd, DndDraft, DndScope, HumanDuration, Image,
        Interaction, Notification, Principal, Priority, Progress, Published, QuietHours, Scope,
        SilenceBody, SilenceScope, State, Status, Ttl, UnreadCount, UserId, WallClock,
    };
}
