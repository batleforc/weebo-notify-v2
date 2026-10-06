//! The ceiling on live connections one principal may hold (design §8.2).
//! Counts connections held at once (not a `RateLimiter` window), per replica (D23): the
//! resource defended is this process's memory and sockets.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use notify_domain::{Principal, limits::MAX_STREAMS_PER_PRINCIPAL};

/// Advisory `Retry-After` for a refused client: a slot frees whenever somebody closes a tab.
pub(crate) const RETRY_AFTER_SECONDS: u32 = 30;

/// Total live connections per replica (SEC-P2-31): past this, shed with a `429` so the
/// client reconnects elsewhere rather than the pod being OOM-killed.
pub(crate) const MAX_STREAMS_PER_REPLICA: usize = 10_000;

/// The live connections this replica is holding, per principal.
#[derive(Debug, Default)]
pub struct StreamSlots {
    /// Absent means zero, so the map stays the size of the connected population.
    held: Mutex<HashMap<Principal, usize>>,
    /// Sum of `held`, kept so `acquire` is O(1); updated under the same lock.
    total: Mutex<usize>,
}

impl StreamSlots {
    /// A registry holding nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a slot for `principal` before serving; `None` means refuse with `429`.
    pub fn acquire(self: &Arc<Self>, principal: &Principal) -> Option<StreamSlot> {
        let mut held = self.lock();
        let mut total = self.lock_total();
        let count = held.get(principal).copied().unwrap_or(0);

        if count >= MAX_STREAMS_PER_PRINCIPAL {
            // Read before insert: a refusal must not allocate a map entry.
            return None;
        }

        if *total >= MAX_STREAMS_PER_REPLICA {
            tracing::warn!(
                total = *total,
                ceiling = MAX_STREAMS_PER_REPLICA,
                "refusing a stream: this replica is at its connection ceiling"
            );
            return None;
        }

        held.insert(principal.clone(), count + 1);
        *total += 1;
        drop(total);
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

    /// How many this replica holds in total. Test and diagnostics only.
    #[must_use]
    pub fn held_total(&self) -> usize {
        *self.lock_total()
    }

    fn release(&self, principal: &Principal) {
        let mut held = self.lock();
        let mut total = self.lock_total();
        *total = total.saturating_sub(1);
        drop(total);
        if let Some(count) = held.get_mut(principal) {
            *count -= 1;
            if *count == 0 {
                // Otherwise the map leaks one entry per principal ever connected.
                held.remove(principal);
            }
        }
    }

    /// The map, recovering a poisoned lock: a counter that is off beats a dead endpoint.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Principal, usize>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Always taken *after* [`Self::lock`], so the two cannot deadlock.
    fn lock_total(&self) -> std::sync::MutexGuard<'_, usize> {
        self.total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One held connection; released on drop, the only construct covering every exit
/// (disconnect, abort, error) so no slot leaks.
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
        // §8.2 limits are per subject, never global.
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
        // Keyed by principal, so a service account is capped too (§5.2bis).
        let slots = Arc::new(StreamSlots::new());
        let alice = somebody();
        let bot = Principal::ServiceAccount {
            id: notify_domain::ServiceAccountId::parse("bot-argocd").unwrap(),
        };

        let _hers: Vec<StreamSlot> = (0..MAX_STREAMS_PER_PRINCIPAL)
            .filter_map(|_| slots.acquire(&alice))
            .collect();

        let first = slots.acquire(&bot);
        assert!(first.is_some(), "a full user does not fill the bot's share");

        let _its: Vec<StreamSlot> = (1..MAX_STREAMS_PER_PRINCIPAL)
            .filter_map(|_| slots.acquire(&bot))
            .collect();
        assert!(slots.acquire(&bot).is_none(), "and its own ceiling too");
    }

    #[test]
    fn a_principal_holding_nothing_leaves_nothing_behind() {
        // This map is keyed on a public endpoint; a stale entry is a free allocation.
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
        // A refusal that incremented would ratchet the ceiling down.
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

#[cfg(test)]
mod ceiling_tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::UserId;

    use super::*;

    fn somebody() -> Principal {
        Principal::User {
            id: UserId::generate(),
        }
    }

    /// Fill the replica using one principal per `MAX_STREAMS_PER_PRINCIPAL`, so the
    /// per-principal cap is never the thing doing the refusing.
    fn fill(slots: &Arc<StreamSlots>, connections: usize) -> Vec<StreamSlot> {
        let mut held = Vec::with_capacity(connections);
        while held.len() < connections {
            let principal = somebody();
            for _ in 0..MAX_STREAMS_PER_PRINCIPAL {
                if held.len() == connections {
                    break;
                }
                match slots.acquire(&principal) {
                    Some(slot) => held.push(slot),
                    None => return held,
                }
            }
        }
        held
    }

    #[test]
    fn a_replica_refuses_past_its_own_ceiling_however_many_principals_ask() {
        let slots = Arc::new(StreamSlots::new());
        let held = fill(&slots, MAX_STREAMS_PER_REPLICA);

        assert_eq!(held.len(), MAX_STREAMS_PER_REPLICA);
        assert_eq!(slots.held_total(), MAX_STREAMS_PER_REPLICA);
        assert!(
            slots.acquire(&somebody()).is_none(),
            "a fresh principal was admitted past the replica ceiling"
        );
    }

    #[test]
    fn the_total_unwinds_completely_when_connections_end() {
        let slots = Arc::new(StreamSlots::new());

        for _ in 0..3 {
            let held = fill(&slots, 512);
            assert_eq!(slots.held_total(), 512);
            drop(held);
            assert_eq!(
                slots.held_total(),
                0,
                "the replica total did not return to zero after every stream ended"
            );
        }

        let after = somebody();
        assert_eq!(slots.held_by(&after), 0);
        assert!(slots.acquire(&after).is_some());
    }

    #[test]
    fn a_refusal_at_the_ceiling_allocates_nothing() {
        // The refusal path is public: it must leave no counter or entry behind.
        let slots = Arc::new(StreamSlots::new());
        let held = fill(&slots, MAX_STREAMS_PER_REPLICA);

        let refused = somebody();
        for _ in 0..1_000 {
            assert!(slots.acquire(&refused).is_none());
        }

        assert_eq!(slots.held_by(&refused), 0);
        assert_eq!(slots.held_total(), MAX_STREAMS_PER_REPLICA);
        drop(held);
        assert_eq!(slots.held_total(), 0);
    }

    #[test]
    fn churn_does_not_drift_the_total() {
        // Open and abandon repeatedly, interleaved (§10 A7).
        let slots = Arc::new(StreamSlots::new());
        let long_lived = fill(&slots, 100);

        for _ in 0..200 {
            let transient = fill(&slots, 50);
            assert_eq!(slots.held_total(), 150);
            drop(transient);
            assert_eq!(slots.held_total(), 100);
        }

        drop(long_lived);
        assert_eq!(slots.held_total(), 0);
    }
}
