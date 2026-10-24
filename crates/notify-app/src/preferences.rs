//! Reading and setting do not disturb (§7.4). The subject is always the caller, so
//! no path takes a user id (T26). Service accounts are refused: no attention to protect.

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
    #[must_use]
    pub fn new(preferences: Arc<dyn PreferenceRepository>) -> Self {
        Self { preferences }
    }

    /// The caller's own preference.
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

    /// Replace the caller's preference wholesale: quiet hours are a list, and merging
    /// a list has no unambiguous meaning.
    pub async fn execute(&self, caller: &Caller, wanted: DndDraft) -> Result<Dnd, Failure> {
        subject(caller)?;
        let user = caller.ensure_person()?;
        let dnd = wanted.for_user(user).validated()?;

        let cursor = self.preferences.set_dnd(&dnd, self.clock.now()).await?;
        // Private to this user (§8.1, T26).
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
            4_096,
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

        assert_eq!(dnd.quiet_hours, [] as [notify_domain::QuietHours; 0]);
        assert!(!dnd.is_active(fixture.clock.now()));
    }

    #[tokio::test]
    async fn a_key_reads_its_owners_preference_but_cannot_change_it() {
        let fixture = fixture();
        let as_key = Caller {
            grant: crate::authz::Grant::ApiKey {
                id: notify_domain::ApiKeyId::generate(),
                scopes: vec![notify_domain::Scope::Publish],
                channels: Vec::new(),
                publish_per_minute: None,
                owner_groups: Vec::new(),
            },
            ..Caller::member(fixture.alice.clone())
        };

        assert!(fixture.read.execute(&as_key).await.is_ok());
        assert!(matches!(
            fixture.set.execute(&as_key, wanted(vec![nights()])).await,
            Err(Failure::Domain(DomainError::SessionRequired))
        ));
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

    /// The draft has no subject field; check it attaches to the caller only.
    #[tokio::test]
    async fn a_preference_is_written_on_the_caller_and_on_nobody_else() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        let Principal::User { id: alice } = fixture.alice else {
            panic!()
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
        // A zone the IANA database does not know.
        let bad = DndDraft {
            quiet_hours: vec![QuietHours {
                timezone: "Europe/Pris".to_owned(),
                ..nights()
            }],
            ..DndDraft::default()
        };

        assert!(fixture.set.execute(&caller, bad).await.is_err());
        assert_eq!(
            fixture.read.execute(&caller).await.unwrap().quiet_hours,
            [] as [notify_domain::QuietHours; 0]
        );
    }

    /// T26: the preference event reaches only this user's other clients.
    #[tokio::test]
    async fn critical_t26_setting_one_announces_it_to_that_user_alone() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        let Principal::User { id: alice } = fixture.alice else {
            panic!()
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
        // `is_muted` must survive the round trip through the port.
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
