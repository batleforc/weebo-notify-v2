//! The ceiling on live connections one principal may hold (design §8.2): counted
//! locally (this process's memory and sockets) and, when clustered, across every
//! replica through [`StreamLeases`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use notify_domain::{Principal, limits::MAX_STREAMS_PER_PRINCIPAL};
use notify_ports::{Acquired, Lease, LeaseRequest, PortResult, StreamLeases};
use tokio::sync::Notify;

/// Advisory `Retry-After` for a refused client: a slot frees whenever somebody closes a tab.
pub(crate) const RETRY_AFTER_SECONDS: u32 = 30;

/// Total live connections per replica (SEC-P2-31): past this, shed with a `429` so the
/// client reconnects elsewhere rather than the pod being OOM-killed.
pub(crate) const MAX_STREAMS_PER_REPLICA: usize = 10_000;

/// The live connections this replica is holding, per principal.
#[derive(Default)]
pub struct StreamSlots {
    /// Absent means zero, so the map stays the size of the connected population.
    held: Mutex<HashMap<Principal, usize>>,
    /// Sum of `held`, kept so `acquire` is O(1); updated under the same lock.
    total: Mutex<usize>,
    /// The instance-wide count, and this replica's name in it.
    cluster: Option<(Arc<dyn StreamLeases>, String)>,
    /// This replica's leases, each with the signal that closes its connection.
    local: Mutex<HashMap<String, Arc<Notify>>>,
}

/// Why a connection was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// At the ceiling (§8.2): `429`, retry later.
    Full,
    /// Disconnected by its owner: `403`, don't retry.
    Disconnected,
}

impl std::fmt::Debug for StreamSlots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamSlots")
            .field("total", &self.held_total())
            .field("clustered", &self.cluster.is_some())
            .finish_non_exhaustive()
    }
}

impl StreamSlots {
    /// A registry holding nothing, counting this replica only.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Also count across replicas: `replica` names this process in `leases`.
    #[must_use]
    pub fn clustered(leases: Arc<dyn StreamLeases>, replica: String) -> Self {
        Self {
            cluster: Some((leases, replica)),
            ..Self::default()
        }
    }

    /// Take the instance-wide lease for a local `slot`. A store outage fails open:
    /// the local ceiling still protects this process.
    pub async fn lease(&self, slot: &mut StreamSlot, request: LeaseRequest) -> Result<(), Refusal> {
        let Some((leases, _)) = &self.cluster else {
            return Ok(());
        };
        match leases.acquire(&request, MAX_STREAMS_PER_PRINCIPAL).await {
            Ok(Acquired::Granted(id)) => {
                let kicked = Arc::new(Notify::new());
                self.local_leases().insert(id.clone(), Arc::clone(&kicked));
                slot.kicked = Some(kicked);
                slot.lease = Some((Arc::clone(leases), id));
                Ok(())
            }
            Ok(Acquired::Full) => Err(Refusal::Full),
            Ok(Acquired::Blocked) => Err(Refusal::Disconnected),
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "stream lease store unavailable; admitting on the local ceiling"
                );
                Ok(())
            }
        }
    }

    /// This replica's name in the lease table, when clustered.
    #[must_use]
    pub fn replica(&self) -> Option<&str> {
        self.cluster.as_ref().map(|(_, replica)| replica.as_str())
    }

    /// Keep this replica's leases alive and close every connection whose lease was
    /// disconnected from another replica.
    pub async fn renew(&self) -> PortResult<()> {
        let Some((leases, replica)) = &self.cluster else {
            return Ok(());
        };
        let live = leases.renew(replica).await?;
        for (id, kicked) in self.local_leases().iter() {
            if !live.contains(id) {
                kicked.notify_one();
            }
        }
        Ok(())
    }

    /// `principal`'s live connections, across every replica.
    pub async fn list(&self, principal: &Principal) -> PortResult<Vec<Lease>> {
        match &self.cluster {
            Some((leases, _)) => leases.list(&principal.reference()).await,
            None => Ok(Vec::new()),
        }
    }

    /// Close one of `principal`'s connections and keep its credential off realtime
    /// until `until`; here at once, elsewhere at that replica's next renewal.
    pub async fn disconnect(
        &self,
        principal: &Principal,
        lease: &str,
        until: time::OffsetDateTime,
    ) -> PortResult<bool> {
        let Some((leases, _)) = &self.cluster else {
            return Ok(false);
        };
        let gone = leases
            .disconnect(&principal.reference(), lease, until)
            .await?;
        if let Some(kicked) = self.local_leases().get(lease) {
            kicked.notify_one();
        }
        Ok(gone)
    }

    fn local_leases(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Notify>>> {
        self.local
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

        held.insert(principal.clone(), count.saturating_add(1));
        *total = total.saturating_add(1);
        drop(total);
        drop(held);

        Some(StreamSlot {
            slots: Arc::clone(self),
            principal: principal.clone(),
            lease: None,
            kicked: None,
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
            *count = count.saturating_sub(1);
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
pub struct StreamSlot {
    slots: Arc<StreamSlots>,
    principal: Principal,
    lease: Option<(Arc<dyn StreamLeases>, String)>,
    kicked: Option<Arc<Notify>>,
}

impl StreamSlot {
    /// Resolves when its owner disconnects this connection; never, without a lease.
    pub async fn kicked(&self) {
        match &self.kicked {
            Some(kicked) => kicked.notified().await,
            None => std::future::pending().await,
        }
    }
}

impl std::fmt::Debug for StreamSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamSlot")
            .field("principal", &self.principal)
            .finish_non_exhaustive()
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.slots.release(&self.principal);
        if let Some((leases, id)) = self.lease.take() {
            self.slots.local_leases().remove(&id);
            // Best effort: a missed release expires with the lease TTL.
            tokio::spawn(async move {
                if let Err(error) = leases.release(&id).await {
                    tracing::warn!(?error, "could not release a stream lease");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
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

    #[tokio::test]
    async fn the_ceiling_holds_across_replicas_and_a_drop_gives_the_lease_back() {
        let leases = Arc::new(notify_ports::test_utils::TestStreamLeases::new());
        let a = Arc::new(StreamSlots::clustered(leases.clone(), "a".to_owned()));
        let b = Arc::new(StreamSlots::clustered(leases.clone(), "b".to_owned()));
        let me = somebody();

        let request = |replica: &str, n: usize| LeaseRequest {
            principal: me.reference(),
            replica: replica.to_owned(),
            transport: "sse".to_owned(),
            client: "test".to_owned(),
            credential: "session".to_owned(),
            fingerprint: format!("token-{n}"),
        };

        let mut held = Vec::new();
        for i in 0..MAX_STREAMS_PER_PRINCIPAL {
            let slots = if i % 2 == 0 { &a } else { &b };
            let mut slot = slots.acquire(&me).unwrap();
            slots.lease(&mut slot, request("x", i)).await.unwrap();
            held.push(slot);
        }

        // Each replica is under the ceiling locally; the instance is not.
        let mut extra = b.acquire(&me).unwrap();
        assert_eq!(
            b.lease(&mut extra, request("b", 99)).await,
            Err(Refusal::Full)
        );

        held.pop();
        tokio::task::yield_now().await;
        assert_eq!(leases.held(), MAX_STREAMS_PER_PRINCIPAL - 1);
        let mut again = a.acquire(&me).unwrap();
        a.lease(&mut again, request("a", 100)).await.unwrap();
    }

    #[tokio::test]
    async fn a_disconnected_connection_closes_and_its_credential_stays_out() {
        let leases = Arc::new(notify_ports::test_utils::TestStreamLeases::new());
        let slots = Arc::new(StreamSlots::clustered(leases.clone(), "a".to_owned()));
        let me = somebody();
        let request = LeaseRequest {
            principal: me.reference(),
            replica: "a".to_owned(),
            transport: "sse".to_owned(),
            client: "phone".to_owned(),
            credential: "session".to_owned(),
            fingerprint: "stolen-phone".to_owned(),
        };
        let mut slot = slots.acquire(&me).unwrap();
        slots.lease(&mut slot, request.clone()).await.unwrap();
        let listed = slots.list(&me).await.unwrap();
        assert_eq!(listed.len(), 1);

        assert!(
            slots
                .disconnect(&me, &listed[0].id, time::OffsetDateTime::now_utc())
                .await
                .unwrap()
        );
        // Resolves at once: the signal was stored for the connection to pick up.
        slot.kicked().await;
        drop(slot);

        let mut retry = slots.acquire(&me).unwrap();
        assert_eq!(
            slots.lease(&mut retry, request).await,
            Err(Refusal::Disconnected)
        );
        assert!(
            !slots
                .disconnect(&somebody(), "1", time::OffsetDateTime::now_utc())
                .await
                .unwrap()
        );
    }
}
