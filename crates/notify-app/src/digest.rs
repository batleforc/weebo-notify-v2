//! The one push that ends a quiet period (design §7.4, §11).
//!
//! §7.4 asks for **a digest, not a burst**: somebody who set quiet hours from ten to
//! eight does not want twelve notifications arriving at 08:00:01, they want one
//! saying "12 notifications while you were away (2 failure)". Same spirit as the
//! "N expired while you were away" counter of §7.2bis, and for the same reason —
//! the information is worth having, the interruption is not worth twelve of.
//!
//! # Why it is a job and not a moment
//!
//! Nothing *happens* when a quiet period ends. A schedule ending is the absence of
//! an event, so somebody has to look — which makes this a timer, and therefore
//! leader-only (§11): every replica running it would send one digest each.
//!
//! # What decides that it is over
//!
//! [`Dnd::is_active`] and nothing else. The store hands back everyone with something
//! held back *and* their preference, and the filtering happens here, because "is this
//! person still not to be disturbed" is a domain question and a store that answered
//! it would be a second implementation of what §7.4 means.

use std::sync::Arc;

use notify_domain::Principal;
use notify_ports::{
    Clock, DeviceRepository, Failure, LeaderElection, PreferenceRepository, PushMessage,
};

use crate::fanout::Fanout;

/// How many tallies to consider per pass. A quiet period ending is not urgent to the
/// minute, and a bounded pass is what lets the loop notice it has lost the lock.
const BATCH: u32 = 200;

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DigestReport {
    /// Digests actually sent.
    pub sent: u64,
    /// Users still inside their quiet period, left for a later pass.
    pub still_quiet: u64,
    /// Whether this replica led. `false` is the normal answer on all but one.
    pub led: bool,
}

/// The periodic do-not-disturb digest (design §7.4).
#[derive(Clone)]
pub struct SendDndDigests {
    preferences: Arc<dyn PreferenceRepository>,
    devices: Arc<dyn DeviceRepository>,
    election: Arc<dyn LeaderElection>,
    fanout: Fanout,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for SendDndDigests {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SendDndDigests").finish_non_exhaustive()
    }
}

impl SendDndDigests {
    /// The name this job takes its lock under.
    pub const JOB: &'static str = "dnd-digest";

    /// Wire it.
    #[must_use]
    pub fn new(
        preferences: Arc<dyn PreferenceRepository>,
        devices: Arc<dyn DeviceRepository>,
        election: Arc<dyn LeaderElection>,
        fanout: Fanout,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            preferences,
            devices,
            election,
            fanout,
            clock,
        }
    }

    /// Run one pass.
    pub async fn execute(&self) -> Result<DigestReport, Failure> {
        let Some(guard) = self.election.try_acquire(Self::JOB).await? else {
            return Ok(DigestReport::default());
        };

        let now = self.clock.now();
        let mut report = DigestReport {
            led: true,
            ..DigestReport::default()
        };

        for tally in self.preferences.deferred_tallies(BATCH).await? {
            // Re-checked per user rather than once at the top: a partition mid-pass
            // must stop this replica before it sends anything else, the same rule
            // the purge follows (scenario T22).
            if !guard.is_held().await {
                tracing::warn!("lost leadership mid-digest; stopping");
                break;
            }

            if tally.dnd.is_active(now) {
                // Still quiet. Leaving the tally alone is the whole point: it keeps
                // counting until there is somebody to tell.
                report.still_quiet += 1;
                continue;
            }

            let message = PushMessage::Digest {
                total: tally.total,
                failures: tally.failures,
            };
            let owner = Principal::User { id: tally.dnd.user };

            for device in self.devices.list_for(&owner).await? {
                if !device.is_active() {
                    continue;
                }
                self.fanout.push_digest(&device, &message).await;
            }

            // Cleared whether or not a device took it. A digest that failed to send
            // must not be re-sent on the next tick: a person who missed one summary
            // has an inbox, and a person who gets the same summary every minute has
            // a bug (§7.4 — the burst this exists to prevent).
            self.preferences.clear_deferred(tally.dnd.user).await?;
            report.sent += 1;
        }

        guard.release().await;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{
        Channel, ChannelId, Device, DeviceId, Dnd, Membership, Scope, Status, UserId,
    };
    use notify_ports::test_utils::{InMemory, TestClock, TestLeaderElection};

    use url::Url;

    use super::*;

    struct Fixture {
        store: InMemory,
        clock: TestClock,
        job: SendDndDigests,
        alice: UserId,
    }

    fn webpush() -> notify_domain::PushTransport {
        notify_domain::PushTransport::WebPush {
            endpoint: Url::parse("https://push.example/abc").unwrap(),
            p256dh: "BPk".to_owned(),
            auth: "k9".to_owned(),
        }
    }

    fn fixture() -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = UserId::generate();
        let owner = Principal::User { id: alice };

        let mut channel = Channel::dynamic(ChannelId::parse("ci-infra").unwrap(), None).unwrap();
        channel.upsert_member(Membership::new(owner.clone(), vec![Scope::Subscribe]).unwrap());
        store.seed_channel(channel);
        store.seed_device(
            Device::enrol(
                DeviceId::generate(),
                owner,
                "phone",
                webpush(),
                None,
                clock.now(),
            )
            .unwrap(),
        );

        let fanout = Fanout::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
        );

        Fixture {
            job: SendDndDigests::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(TestLeaderElection::leader()),
                fanout,
                Arc::new(clock.clone()),
            ),
            store,
            clock,
            alice,
        }
    }

    /// Put `count` deferrals on the tally, of which `failures` closed on a failure.
    async fn defer(fixture: &Fixture, count: u32, failures: u32) {
        for index in 0..count {
            let status = if index < failures {
                Status::Failure
            } else {
                Status::Info
            };
            PreferenceRepository::defer(&fixture.store, fixture.alice, status)
                .await
                .unwrap();
        }
    }

    async fn quiet_until(fixture: &Fixture, hours: i64) {
        PreferenceRepository::set_dnd(
            &fixture.store,
            &Dnd {
                until: Some(fixture.clock.now() + time::Duration::hours(hours)),
                ..Dnd::off(fixture.alice)
            },
            fixture.clock.now(),
        )
        .await
        .unwrap();
    }

    // --- T19 · One digest, not a burst (design §3.4, §7.4) ----------------

    #[tokio::test]
    async fn critical_t19_leaving_a_quiet_period_sends_exactly_one_digest() {
        let fixture = fixture();
        // The period is already over; twelve things happened during it.
        quiet_until(&fixture, -1).await;
        defer(&fixture, 12, 2).await;

        let report = fixture.job.execute().await.unwrap();

        assert!(report.led);
        assert_eq!(report.sent, 1);
        let pushes = fixture.store.pushes();
        assert_eq!(
            pushes.len(),
            1,
            "one push, not twelve — that is the feature"
        );
        assert_eq!(
            pushes[0].1,
            PushMessage::Digest {
                total: 12,
                failures: 2
            }
        );
    }

    #[tokio::test]
    async fn critical_t19_nothing_is_sent_while_the_quiet_period_is_still_running() {
        let fixture = fixture();
        quiet_until(&fixture, 8).await;
        defer(&fixture, 3, 0).await;

        let report = fixture.job.execute().await.unwrap();

        assert_eq!(report.sent, 0);
        assert_eq!(report.still_quiet, 1);
        assert!(
            fixture.store.pushes().is_empty(),
            "telling somebody mid-silence would be the interruption they turned off"
        );
    }

    #[tokio::test]
    async fn a_digest_is_sent_once_and_not_again() {
        let fixture = fixture();
        quiet_until(&fixture, -1).await;
        defer(&fixture, 4, 1).await;

        fixture.job.execute().await.unwrap();
        let second = fixture.job.execute().await.unwrap();

        assert_eq!(second.sent, 0);
        assert_eq!(fixture.store.pushes().len(), 1);
    }

    #[tokio::test]
    async fn changing_the_preference_drops_what_was_counted_under_the_old_one() {
        let fixture = fixture();
        quiet_until(&fixture, 8).await;
        defer(&fixture, 5, 0).await;

        // Redefining it: a digest counting the old period would be reporting
        // something that, under the new preference, did not happen.
        quiet_until(&fixture, -1).await;

        assert_eq!(fixture.job.execute().await.unwrap().sent, 0);
        assert!(fixture.store.pushes().is_empty());
    }

    #[tokio::test]
    async fn a_replica_that_is_not_leading_does_nothing() {
        let fixture = fixture();
        quiet_until(&fixture, -1).await;
        defer(&fixture, 2, 0).await;

        let idle = SendDndDigests::new(
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.store.clone()),
            Arc::new(TestLeaderElection::follower()),
            Fanout::new(
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.clock.clone()),
            ),
            Arc::new(fixture.clock.clone()),
        );

        let report = idle.execute().await.unwrap();

        assert!(!report.led, "not leading is a normal answer, not a failure");
        assert!(fixture.store.pushes().is_empty());
    }

    #[tokio::test]
    async fn a_stream_only_device_is_not_digested_at() {
        // It was never pushed to in the first place, so there is nothing it missed.
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = UserId::generate();
        let owner = Principal::User { id: alice };

        store.seed_device(
            Device::enrol(
                DeviceId::generate(),
                owner,
                "laptop",
                notify_domain::PushTransport::Stream,
                None,
                clock.now(),
            )
            .unwrap(),
        );
        PreferenceRepository::set_dnd(
            &store,
            &Dnd {
                until: Some(clock.now() - time::Duration::hours(1)),
                ..Dnd::off(alice)
            },
            clock.now(),
        )
        .await
        .unwrap();
        PreferenceRepository::defer(&store, alice, Status::Info)
            .await
            .unwrap();

        let job = SendDndDigests::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(TestLeaderElection::leader()),
            Fanout::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            Arc::new(clock.clone()),
        );

        job.execute().await.unwrap();

        assert!(store.pushes().is_empty());
    }
}
