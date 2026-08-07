//! The ceiling on live connections one principal may hold (design §8.2).
//!
//! §8.2 publishes "connexions SSE/WS par user: 20 — multi-onglets + devices", and
//! until this module existed the number was published and nothing read it: both
//! transports opened as many connections as anybody asked for. That is the same
//! shape as the `MAX_PAYLOAD_BYTES` finding of M4duodecies — a documented limit is
//! a limit somebody relies on, whether or not anything enforces it.
//!
//! # Why this is not the `RateLimiter` port
//!
//! [`notify_ports::RateLimiter`] counts *requests over a window* and every other
//! number in §8.2 is one of those. This one is not: it bounds how many connections
//! are **held at the same time**, and consuming a token per connect would cap
//! connection *attempts per minute* instead — a client that reconnects all day
//! within its budget would still accumulate sockets forever, which is precisely
//! the failure this prevents. Different question, different mechanism.
//!
//! # Why the count is per replica
//!
//! It is a `HashMap` in this process, so with several replicas behind a load
//! balancer one principal can hold `20 × replicas` in total (D23). That is a real
//! gap against a strict reading of §8.2 and it is written down rather than
//! rounded off — but it is a much smaller one than it looks, because of what the
//! cap is *for*: a held connection costs the memory, the task and the file
//! descriptor of **the process holding it**. A per-replica ceiling bounds exactly
//! that, on every replica, which is the resource being defended. The instance-wide
//! reading would need shared state on the connect path, and paying a round trip
//! per connection to make a number tidier is not a trade worth taking blind.
//!
//! Contrast [`notify_ports::RateLimiter`], whose port documentation makes the
//! opposite call for publishing: a publish rate protects a **shared** downstream —
//! the fan-out, the database — so an in-process counter there really would enforce
//! `n × 60/min` against a resource that only has `60/min` of room. The two limits
//! read alike in a table and defend different things.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use notify_domain::{Principal, limits::MAX_STREAMS_PER_PRINCIPAL};

/// How long a refused client is told to wait, in seconds.
///
/// Advisory, and unlike a rate-limit window it is not computed: a slot frees when
/// somebody closes a tab, and nothing here knows when that will be. A `429`
/// without `Retry-After` turns a polite client into a busy loop, so it gets a
/// number; the number is a suggestion.
pub(crate) const RETRY_AFTER_SECONDS: u32 = 30;

/// The live connections this replica is holding, per principal.
#[derive(Debug, Default)]
pub struct StreamSlots {
    /// Absent means zero: a principal with no connection has no entry, so the map
    /// stays the size of the connected population rather than of the user table.
    held: Mutex<HashMap<Principal, usize>>,
}

impl StreamSlots {
    /// A registry holding nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a slot for `principal`, or refuse.
    ///
    /// `None` is the refusal; the caller turns it into the `429` §8.2 asks for.
    /// Counting happens **before** the connection is served rather than after, so
    /// the twenty-first client is refused instead of being served and then found
    /// to be one too many.
    pub fn acquire(self: &Arc<Self>, principal: &Principal) -> Option<StreamSlot> {
        let mut held = self.lock();
        let count = held.get(principal).copied().unwrap_or(0);

        if count >= MAX_STREAMS_PER_PRINCIPAL {
            // Read before insert, so a refusal writes nothing: this map is keyed by
            // principal on a public endpoint, and a refusal that left a row behind
            // would be an allocation anybody could make by being refused.
            return None;
        }

        held.insert(principal.clone(), count + 1);
        drop(held);

        Some(StreamSlot {
            slots: Arc::clone(self),
            principal: principal.clone(),
        })
    }

    /// How many this replica holds for one principal. Test and diagnostics only.
    #[must_use]
    pub fn held_by(&self, principal: &Principal) -> usize {
        self.lock().get(principal).copied().unwrap_or(0)
    }

    fn release(&self, principal: &Principal) {
        let mut held = self.lock();
        if let Some(count) = held.get_mut(principal) {
            *count -= 1;
            if *count == 0 {
                // Or the map grows once per principal that has ever connected and
                // never shrinks — a slow leak on a long-lived replica.
                held.remove(principal);
            }
        }
    }

    /// The map, with a poisoned lock recovered rather than propagated.
    ///
    /// A panic while holding this lock would otherwise refuse every subsequent
    /// connection on the replica for the rest of its life. The data behind it is a
    /// counter: the worst a poisoned one can be is off, and a count that is off is
    /// better than a stream endpoint that is down.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Principal, usize>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One held connection. Releases on drop, and that is the whole design.
///
/// A stream ends when the client goes away, when the task is aborted at shutdown,
/// or when the event log becomes unreadable. Releasing on a handler's success path
/// would leak a slot on every one of those, and a leaked slot is worse than no cap
/// at all: it is a user locked out of their own inbox until the replica restarts.
/// `Drop` is the only construct that covers every exit, which is the same reason
/// the `Connected` metrics guard next door is shaped this way.
#[derive(Debug)]
pub struct StreamSlot {
    slots: Arc<StreamSlots>,
    principal: Principal,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.slots.release(&self.principal);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::UserId;

    use super::*;

    /// A fresh person. Two calls are two different people.
    fn somebody() -> Principal {
        Principal::User {
            id: UserId::generate(),
        }
    }

    #[test]
    fn the_twenty_first_connection_is_refused() {
        let slots = Arc::new(StreamSlots::new());
        let alice = somebody();

        let held: Vec<StreamSlot> = (0..MAX_STREAMS_PER_PRINCIPAL)
            .filter_map(|_| slots.acquire(&alice))
            .collect();

        assert_eq!(
            held.len(),
            MAX_STREAMS_PER_PRINCIPAL,
            "every connection up to the ceiling is served"
        );
        assert!(
            slots.acquire(&alice).is_none(),
            "the ceiling of design §8.2 is a ceiling"
        );
    }

    #[test]
    fn closing_a_connection_gives_the_slot_back() {
        // The half that matters. A cap whose slots are never returned is not a cap,
        // it is a user locked out of their inbox one tab at a time.
        let slots = Arc::new(StreamSlots::new());
        let alice = somebody();

        let mut held: Vec<StreamSlot> = (0..MAX_STREAMS_PER_PRINCIPAL)
            .filter_map(|_| slots.acquire(&alice))
            .collect();
        assert!(slots.acquire(&alice).is_none());

        held.pop();

        let reopened = slots.acquire(&alice);
        assert!(reopened.is_some(), "a closed tab frees the slot it held");
    }

    #[test]
    fn one_principal_filling_up_does_not_reach_another() {
        // The property that makes this a cap and not an outage: §8.2's limits are
        // per subject, "never global" (the `RateLimiter` port says why — one noisy
        // client must degrade itself and nobody else).
        let slots = Arc::new(StreamSlots::new());
        let alice = somebody();
        let bob = somebody();

        let _hers: Vec<StreamSlot> = (0..MAX_STREAMS_PER_PRINCIPAL)
            .filter_map(|_| slots.acquire(&alice))
            .collect();

        assert!(slots.acquire(&alice).is_none());
        assert!(
            slots.acquire(&bob).is_some(),
            "somebody else's tabs are not this caller's problem"
        );
    }

    #[test]
    fn a_service_account_gets_its_own_ceiling() {
        // §8.2 says "user"; §5.2bis lets a service account hold a connection and a
        // sync cursor of its own. Keying by principal rather than by user is what
        // stops a bot in a reconnect loop from being unbounded.
        let slots = Arc::new(StreamSlots::new());
        let alice = somebody();
        let bot = Principal::ServiceAccount {
            id: notify_domain::ServiceAccountId::parse("bot-argocd").unwrap(),
        };

        let _hers: Vec<StreamSlot> = (0..MAX_STREAMS_PER_PRINCIPAL)
            .filter_map(|_| slots.acquire(&alice))
            .collect();

        // Bound, not asserted-and-dropped: a slot released on the same line would
        // leave the bot holding nothing and the count below would be off by one.
        let first = slots.acquire(&bot);
        assert!(first.is_some(), "a full user does not fill the bot's share");

        let _its: Vec<StreamSlot> = (1..MAX_STREAMS_PER_PRINCIPAL)
            .filter_map(|_| slots.acquire(&bot))
            .collect();
        assert!(slots.acquire(&bot).is_none(), "and its own ceiling too");
    }

    #[test]
    fn a_principal_holding_nothing_leaves_nothing_behind() {
        // Not cosmetic: this map is keyed by principal on a public endpoint, so an
        // entry that outlives its connections is a per-caller allocation anybody
        // can create by connecting and leaving.
        let slots = Arc::new(StreamSlots::new());
        let alice = somebody();

        let one = slots.acquire(&alice).unwrap();
        assert_eq!(slots.held_by(&alice), 1);
        drop(one);

        assert_eq!(slots.held_by(&alice), 0);
        assert!(
            slots.lock().is_empty(),
            "a principal who has gone leaves no row"
        );
    }

    #[test]
    fn a_refusal_counts_nothing() {
        // A refusal that incremented would make the ceiling ratchet downwards: a
        // client retrying against a full replica would lower its own limit, and the
        // slots it does hold would never be enough to get back under it.
        let slots = Arc::new(StreamSlots::new());
        let alice = somebody();

        let _held: Vec<StreamSlot> = (0..MAX_STREAMS_PER_PRINCIPAL)
            .filter_map(|_| slots.acquire(&alice))
            .collect();

        assert!(slots.acquire(&alice).is_none());
        assert_eq!(
            slots.held_by(&alice),
            MAX_STREAMS_PER_PRINCIPAL,
            "a refusal counts nothing, in either direction"
        );
    }
}
