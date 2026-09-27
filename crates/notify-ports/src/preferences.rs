//! Do-not-disturb, stored per user (design §7.4).
//!
//! Its own port rather than a third method on [`InteractionRepository`]: a silence
//! is set from a button on a notification and belongs next to the interactions it
//! arrives with, while a do-not-disturb is set in Settings and outlives every
//! notification in the instance. They meet on the wire, in the `preference` event
//! of §8.1, and nowhere else.
//!
//! # Writing one appends its event
//!
//! Same rule as everywhere else ([ADR-0012]): the row and the event row are one
//! transaction, so the write is the only place that knows where it landed and
//! returns the cursor. The event carries **both** halves of the preference — the
//! new do-not-disturb and the silences still in force — because that is what §8.1
//! promises a client, and an event that carried only the half that changed would
//! leave the other one to be guessed.
//!
//! [`InteractionRepository`]: crate::interactions::InteractionRepository
//! [ADR-0012]: https://github.com/batleforc/weebo-notify-v2/blob/main/docs/content/docs/adr/0012-single-events-table-as-the-cursor.mdx

use notify_domain::{Cursor, Dnd, Status, UserId};
use time::OffsetDateTime;

use crate::error::PortResult;

/// What one user has had held back, and the preference that says whether it is time
/// to tell them (design §7.4).
///
/// The preference travels with the count on purpose: deciding *whether the quiet
/// period is over* is [`Dnd::is_active`], a domain question, and a store that
/// answered it would be a second implementation of what §7.4 means — the thing the
/// domain module exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredTally {
    /// Whose, and the schedule that decides when the digest goes out.
    pub dnd: Dnd,
    /// How many pushes were held back.
    pub total: u32,
    /// How many of those closed on a failure — the one status a digest calls out
    /// (design §7.4).
    pub failures: u32,
}

/// The per-user delivery preference.
#[async_trait::async_trait]
pub trait PreferenceRepository: Send + Sync {
    /// What this user has set.
    ///
    /// Returns [`Dnd::off`] rather than `None` for somebody who has never opened
    /// Settings: "no preference" and "a preference that mutes nothing" are the same
    /// thing to every caller, and making them ask twice is how one of them forgets.
    async fn dnd(&self, user: UserId) -> PortResult<Dnd>;

    /// Replace this user's do-not-disturb and announce it.
    ///
    /// `now` is the moment the resulting `preference` event is stamped with, and
    /// the moment the silences it carries are judged active at.
    ///
    /// Replacing a preference **clears the held-back tally**: a digest for a quiet
    /// period somebody has just redefined would be counting something that no longer
    /// happened.
    async fn set_dnd(&self, dnd: &Dnd, now: OffsetDateTime) -> PortResult<Cursor>;

    /// Record that one more push was held back for this user (design §7.4).
    ///
    /// A counter and not a row per notification: the digest says "12 notifications
    /// (2 failure)", so two integers answer it, and a table of held-back
    /// notifications would grow on the fan-out path to be read once and thrown away.
    /// What is lost is the ability to *list* them later, which no screen asks for —
    /// the inbox already holds them.
    ///
    /// A user with no stored preference cannot be muted, so this matching no row is
    /// the correct outcome rather than something to report.
    async fn defer(&self, user: UserId, status: Status) -> PortResult<()>;

    /// Everyone with something held back, with their preference (design §7.4).
    ///
    /// Returns them whether or not their quiet period is over: the caller decides
    /// that, because it is a domain question and the store has no business
    /// answering it.
    ///
    /// Ordered by user and paged by `after`, the last user of the previous page.
    /// A caller has to be able to walk past a page full of people who are still
    /// quiet — they keep their tallies, so without a way past them the same page
    /// comes back on every pass and nobody behind it ever gets a digest.
    async fn deferred_tallies(
        &self,
        after: Option<UserId>,
        limit: u32,
    ) -> PortResult<Vec<DeferredTally>>;

    /// Take a delivered digest off a user's tally.
    ///
    /// Subtracts what the digest reported rather than zeroing: a push held back
    /// between reading the tally and sending the digest was not in it, and must
    /// still be counted in the next one.
    async fn clear_deferred(&self, user: UserId, total: u32, failures: u32) -> PortResult<()>;
}
