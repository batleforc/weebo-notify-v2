//! Reading and setting do not disturb (design §7.4, §8 `/preferences/dnd`).
//!
//! Two use cases and one rule that runs through both: **a preference belongs to the
//! person it is about, and to nobody else.** There is no path here that takes a user
//! id — the subject is always the caller. That is what makes scenario T26 a property
//! of the code rather than a check somebody has to remember at each endpoint, and it
//! is why neither of these consults [`Authorizer`](crate::authz::Authorizer): there
//! is no channel involved and therefore no membership to test.
//!
//! A service account is refused rather than given an empty preference. A bot has no
//! attention to protect (§5.2bis), and answering "your do-not-disturb is off" to one
//! would suggest it has a setting to change.

use std::sync::Arc;

use notify_domain::{Dnd, DndDraft, DomainError, Principal, UserId};
use notify_ports::{Clock, Failure, PreferenceRepository};

use crate::authz::Caller;
use crate::fanout::Fanout;

/// `GET /api/v1/preferences/dnd` (design §7.4).
#[derive(Clone)]
pub struct ReadDnd {
    preferences: Arc<dyn PreferenceRepository>,
}

impl core::fmt::Debug for ReadDnd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadDnd").finish_non_exhaustive()
    }
}

impl ReadDnd {
    /// Wire it.
    #[must_use]
    pub fn new(preferences: Arc<dyn PreferenceRepository>) -> Self {
        Self { preferences }
    }

    /// The caller's own preference. Never anybody else's — there is no argument for
    /// it to be.
    pub async fn execute(&self, caller: &Caller) -> Result<Dnd, Failure> {
        Ok(self.preferences.dnd(subject(caller)?).await?)
    }
}

/// `PUT /api/v1/preferences/dnd` (design §7.4).
#[derive(Clone)]
pub struct SetDnd {
    preferences: Arc<dyn PreferenceRepository>,
    clock: Arc<dyn Clock>,
    fanout: Fanout,
}

impl core::fmt::Debug for SetDnd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SetDnd").finish_non_exhaustive()
    }
}

impl SetDnd {
    /// Wire it.
    #[must_use]
    pub fn new(
        preferences: Arc<dyn PreferenceRepository>,
        clock: Arc<dyn Clock>,
        fanout: Fanout,
    ) -> Self {
        Self {
            preferences,
            clock,
            fanout,
        }
    }

    /// Replace the caller's preference wholesale.
    ///
    /// Wholesale, not a patch: the draft **is** the new state (§7.4 carries it whole
    /// on the wire for the same reason). Quiet hours are a list, and merging a list
    /// has no unambiguous meaning — a partial update that dropped a window would be
    /// indistinguishable from one that left it alone.
    ///
    /// It takes a [`DndDraft`] and not a [`Dnd`] because the subject is not an
    /// input: this is the only place that knows whose preference is being written.
    pub async fn execute(&self, caller: &Caller, wanted: DndDraft) -> Result<Dnd, Failure> {
        let user = subject(caller)?;
        let dnd = wanted.for_user(user).validated()?;

        let cursor = self.preferences.set_dnd(&dnd, self.clock.now()).await?;
        // Private to this user: their other clients apply it immediately, nobody
        // else's hears about it (design §8.1, scenario T26).
        self.fanout.poke_user(user, cursor).await;

        Ok(dnd)
    }
}

/// Whose preference a call is about.
fn subject(caller: &Caller) -> Result<UserId, Failure> {
    match caller.principal {
        Principal::User { id } => Ok(id),
        _ => Err(DomainError::Malformed {
            field: "preferences",
            reason: "only a user has a do-not-disturb to set",
        }
        .into()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        reason = "a failing assertion is a failing test"
    )]

    use notify_domain::{
        ChannelId, Day, DndScope, Priority, QuietHours, ServiceAccountId, WallClock,
    };
    use notify_ports::test_utils::{InMemory, TestClock};

    use super::*;

    struct Fixture {
        store: InMemory,
        clock: TestClock,
        read: ReadDnd,
        set: SetDnd,
        alice: Principal,
    }

    fn fixture() -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();
        let fanout = Fanout::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
        );

        Fixture {
            read: ReadDnd::new(Arc::new(store.clone())),
            set: SetDnd::new(Arc::new(store.clone()), Arc::new(clock.clone()), fanout),
            alice: Principal::User {
                id: UserId::generate(),
            },
            store,
            clock,
        }
    }

    fn nights() -> QuietHours {
        QuietHours::new(
            vec![Day::Mon],
            WallClock::new(22, 0).unwrap(),
            WallClock::new(8, 0).unwrap(),
            "Europe/Paris",
        )
        .unwrap()
    }

    fn wanted(quiet_hours: Vec<QuietHours>) -> DndDraft {
        DndDraft {
            quiet_hours,
            ..DndDraft::default()
        }
    }

    #[tokio::test]
    async fn somebody_who_never_set_one_reads_a_preference_that_mutes_nothing() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());

        let dnd = fixture.read.execute(&caller).await.unwrap();

        assert!(dnd.quiet_hours.is_empty());
        assert!(!dnd.is_active(fixture.clock.now()));
    }

    #[tokio::test]
    async fn setting_one_stores_it_and_reads_back() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());

        let stored = fixture
            .set
            .execute(&caller, wanted(vec![nights()]))
            .await
            .unwrap();

        assert_eq!(fixture.read.execute(&caller).await.unwrap(), stored);
    }

    /// The subject is not an input — a draft has no field for it — so this checks
    /// the other half: the one it *does* get attached to.
    #[tokio::test]
    async fn a_preference_is_written_on_the_caller_and_on_nobody_else() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        let Principal::User { id: alice } = fixture.alice else {
            unreachable!()
        };
        let somebody_else = UserId::generate();

        let stored = fixture
            .set
            .execute(&caller, wanted(vec![nights()]))
            .await
            .unwrap();

        assert_eq!(stored.user, alice, "a preference is set on its own owner");
        assert!(
            fixture
                .store
                .dnd(somebody_else)
                .await
                .unwrap()
                .quiet_hours
                .is_empty(),
            "and never on anybody else's"
        );
    }

    #[tokio::test]
    async fn a_preference_that_would_not_resolve_is_refused_before_it_is_stored() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        // A zone the IANA database does not know: accepted today, a notification
        // that never arrives in six months.
        let bad = DndDraft {
            quiet_hours: vec![QuietHours {
                timezone: "Europe/Pris".to_owned(),
                ..nights()
            }],
            ..DndDraft::default()
        };

        assert!(fixture.set.execute(&caller, bad).await.is_err());
        assert!(
            fixture
                .read
                .execute(&caller)
                .await
                .unwrap()
                .quiet_hours
                .is_empty()
        );
    }

    /// T26: the preference event is private, and it is what the other clients of
    /// *this* user hear.
    #[tokio::test]
    async fn critical_t26_setting_one_announces_it_to_that_user_alone() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        let Principal::User { id: alice } = fixture.alice else {
            unreachable!()
        };

        fixture
            .set
            .execute(&caller, wanted(vec![nights()]))
            .await
            .unwrap();

        let pokes = fixture.store.pokes();
        assert_eq!(pokes.len(), 1);
        assert!(
            matches!(&pokes[0], notify_ports::Poke::User { id, .. } if *id == alice),
            "a preference is addressed to its owner, not to a channel: {pokes:?}"
        );
    }

    #[tokio::test]
    async fn the_announcement_carries_both_halves_of_stop_notifying_me() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());

        fixture
            .set
            .execute(
                &caller,
                DndDraft {
                    exceptions: vec![ChannelId::parse("cluster-ovh-1").unwrap()],
                    scope: DndScope::Channels {
                        channels: vec![ChannelId::parse("ci-infra").unwrap()],
                    },
                    ..wanted(vec![nights()])
                },
            )
            .await
            .unwrap();

        let events = fixture.store.events();
        let notify_ports::EventPayload::Preference(preference) = &events.last().unwrap().payload
        else {
            panic!("the last event is the preference that was just set");
        };
        assert_eq!(preference.dnd.exceptions.len(), 1);
        assert!(
            preference.silences.is_empty(),
            "the silences half is carried too, empty or not"
        );
    }

    #[tokio::test]
    async fn a_service_account_has_no_attention_to_protect() {
        let fixture = fixture();
        let bot = Caller::member(Principal::ServiceAccount {
            id: ServiceAccountId::parse("bot-argocd").unwrap(),
        });

        assert!(fixture.read.execute(&bot).await.is_err());
        assert!(fixture.set.execute(&bot, wanted(Vec::new())).await.is_err());
    }

    #[tokio::test]
    async fn what_was_stored_is_what_the_fanout_will_ask_about() {
        // The point of storing it: `is_muted` is the one question, and it has to
        // survive the round trip through the port unchanged.
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        let ci = ChannelId::parse("ci-infra").unwrap();

        fixture
            .set
            .execute(
                &caller,
                DndDraft {
                    until: Some(fixture.clock.now() + time::Duration::hours(2)),
                    ..wanted(Vec::new())
                },
            )
            .await
            .unwrap();

        let stored = fixture.read.execute(&caller).await.unwrap();
        assert!(stored.is_muted(&ci, Priority::Default, fixture.clock.now()));
        assert!(!stored.is_muted(
            &ci,
            Priority::Default,
            fixture.clock.now() + time::Duration::hours(3)
        ));
    }
}
