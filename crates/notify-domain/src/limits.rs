//! Every ceiling the domain enforces, in one place.
//!
//! Two provenances, kept visually distinct because they age differently:
//!
//! * **Contract** — published in design §8.2 and quoted in the docs site. Changing
//!   one of these is a product decision and needs the design document updated.
//! * **Guard rail** — not in the design, chosen here so that no text field is
//!   unbounded. These exist to stop a publisher from turning an inbox into a
//!   denial of service, and can be adjusted without touching the contract.
//!
//! Text ceilings count **characters, not bytes** — a limit a user is told about
//! should be one they can count, and it is measured *after* sanitisation (design
//! §2.3) so escape sequences never eat someone's title budget.

use core::time::Duration;

// --- Contract (design §8.2) ------------------------------------------------

/// Serialised payload ceiling, attachments excluded. Enforced where the bytes
/// actually exist (the HTTP adapter); exposed here so there is a single source.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// Inline `data:` image ceiling (design §2.3). Past this, publishers upload an
/// attachment and reference it instead.
pub const MAX_DATA_URI_BYTES: usize = 48 * 1024;

/// Attachment ceiling (design §8.2): a presign asked for more is refused.
///
/// Checked **before** signing rather than after uploading, which is the whole
/// difference between a limit and a wish: a presigned URL is a bearer credential
/// for the bucket, so one issued for an oversized object is one we handed out
/// against our own rule and cannot take back.
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

/// Actions per notification. The array order is display priority: mobile push
/// transports only surface the first two or three (design §2.3).
pub const MAX_ACTIONS: usize = 5;

/// Revisions per notification. The 1 001st is refused and the notification is
/// left intact (design §8.2, scenario T6).
pub const MAX_REVISIONS: u32 = 1_000;

/// Live connections — SSE and WebSocket together — one principal may hold at
/// once (design §8.2, "connexions SSE/WS par user": 20, multi-tab plus devices).
///
/// Enforced where the connections are held (the HTTP adapter), like
/// [`MAX_PAYLOAD_BYTES`] is enforced where the bytes are; exposed here so the
/// number has one home.
///
/// **Per principal, not per user**, and the widening is deliberate: §8.2 says
/// "user" because a person with tabs and devices is the case it was written for,
/// but a service account holds connections too (design §5.2bis) and an unbounded
/// bot is the same leak with less warning. The four principals of §5 each get
/// their own ceiling.
///
/// **Per replica, not per instance** — see D23. What this guarantees is that no
/// single process holds more than this many connections for one principal, which
/// is the resource the cap protects; the instance-wide reading of §8.2 needs
/// shared state that does not exist here.
pub const MAX_STREAMS_PER_PRINCIPAL: usize = 20;

/// Action label ceiling (design §2.3).
pub const MAX_LABEL_CHARS: usize = 40;

// --- Guard rails (chosen here, not in the design) --------------------------

/// Title ceiling. Long enough for `cluster-ovh-1 · etcd degraded` and a bit more;
/// anything longer belongs in the body.
pub const MAX_TITLE_CHARS: usize = 256;

/// Body ceiling. Sits well under [`MAX_PAYLOAD_BYTES`] so a body alone can never
/// be the reason a payload is refused.
pub const MAX_BODY_CHARS: usize = 16 * 1024;

/// Icon name ceiling — an identifier, not prose.
pub const MAX_ICON_CHARS: usize = 64;

/// Tags per notification.
pub const MAX_TAGS: usize = 16;

/// Length of a single tag.
pub const MAX_TAG_CHARS: usize = 32;

/// The ceiling on an avatar upload (design §2.4, §8.2).
///
/// Here rather than in the decoder because it is a documented API limit like every
/// other number in this file — a client has to be able to refuse a file before
/// spending an upload on it.
pub const MAX_AVATAR_BYTES: usize = 512 * 1024;

/// Length of a `group_key` (design §2.3) — clients group on it, they do not read it.
pub const MAX_GROUP_KEY_CHARS: usize = 128;

/// Options offered by one `choice` action. Past this it is a form, not a
/// notification button.
pub const MAX_CHOICE_OPTIONS: usize = 10;

/// Placeholder text of a `reply` action — a hint, not instructions.
pub const MAX_PLACEHOLDER_CHARS: usize = 120;

/// Free text a user can send back through a `reply` interaction.
pub const MAX_REPLY_CHARS: usize = 4_096;

/// Length of a machine-facing key: action keys, choice option keys, channel ids.
pub const MAX_KEY_CHARS: usize = 64;

/// Length of a publisher-supplied `external_id` — CI run ids, job names.
pub const MAX_EXTERNAL_ID_CHARS: usize = 128;

/// Ceiling on any duration a publisher can name (TTL, silence): ten years. Not a
/// policy, just the point past which a value is a unit mistake rather than an
/// intent.
pub const MAX_DURATION: Duration = Duration::from_secs(10 * 365 * 24 * 3_600);
