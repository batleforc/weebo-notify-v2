//! Every ceiling the domain enforces. **Contract** limits are published in §8.2;
//! **guard rails** are local choices. Text ceilings count characters after sanitisation.

use core::time::Duration;

// --- Contract (design §8.2) ------------------------------------------------

/// Serialised payload ceiling, attachments excluded; enforced by the HTTP adapter.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// Inline `data:` image ceiling (§2.3); larger images must be attachments.
pub const MAX_DATA_URI_BYTES: usize = 48 * 1024;

/// Attachment ceiling (§8.2), checked before signing a presign.
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

/// Actions per notification; array order is display priority (§2.3).
pub const MAX_ACTIONS: usize = 5;

/// Revisions per notification; the 1 001st is refused (§8.2, T6).
pub const MAX_REVISIONS: u32 = 1_000;

/// Live SSE+WS connections one principal may hold per replica (§8.2, D23);
/// enforced by the HTTP adapter.
pub const MAX_STREAMS_PER_PRINCIPAL: usize = 20;

/// Action label ceiling (design §2.3).
pub const MAX_LABEL_CHARS: usize = 40;

// --- Guard rails (chosen here, not in the design) --------------------------

/// Title ceiling.
pub const MAX_TITLE_CHARS: usize = 256;

/// Body ceiling, well under [`MAX_PAYLOAD_BYTES`].
pub const MAX_BODY_CHARS: usize = 16 * 1024;

/// Icon name ceiling — an identifier, not prose.
pub const MAX_ICON_CHARS: usize = 64;

/// Tags per notification.
pub const MAX_TAGS: usize = 16;

/// Length of a single tag.
pub const MAX_TAG_CHARS: usize = 32;

/// The ceiling on an avatar upload (§2.4, §8.2).
pub const MAX_AVATAR_BYTES: usize = 512 * 1024;

/// Length of a `group_key` (design §2.3) — clients group on it, they do not read it.
pub const MAX_GROUP_KEY_CHARS: usize = 128;

/// Options offered by one `choice` action.
pub const MAX_CHOICE_OPTIONS: usize = 10;

/// Placeholder text of a `reply` action — a hint, not instructions.
pub const MAX_PLACEHOLDER_CHARS: usize = 120;

/// Free text a user can send back through a `reply` interaction.
pub const MAX_REPLY_CHARS: usize = 4_096;

/// Length of a machine-facing key: action keys, choice option keys, channel ids.
pub const MAX_KEY_CHARS: usize = 64;

/// Length of a publisher-supplied `external_id` — CI run ids, job names.
pub const MAX_EXTERNAL_ID_CHARS: usize = 128;

/// Ceiling on any duration a publisher can name (TTL, silence): ten years.
pub const MAX_DURATION: Duration = Duration::from_secs(10 * 365 * 24 * 3_600);
