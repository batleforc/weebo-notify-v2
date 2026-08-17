//! A clock a test drives.

use std::sync::{Arc, Mutex, PoisonError};

use time::{Duration, OffsetDateTime};

use crate::clock::Clock;

/// A clock that only moves when a test moves it.
///
/// Which is what makes "the notification expires in 30 days" a microsecond of test
/// rather than a scenario nobody writes.
#[derive(Debug, Clone)]
pub struct TestClock {
    now: Arc<Mutex<OffsetDateTime>>,
}

impl TestClock {
    /// A fixed, arbitrary, readable instant: 2026-08-02T00:00:00Z.
    ///
    /// A constant rather than "now" so a failing test is reproducible a month later,
    /// and so nothing accidentally depends on the day of the week it was run.
    pub const EPOCH: i64 = 1_785_974_400;

    /// A clock parked at [`Self::EPOCH`].
    pub fn new() -> Self {
        Self::at(Self::default_start())
    }

    /// A clock parked at a chosen instant.
    pub fn at(now: OffsetDateTime) -> Self {
        Self {
            now: Arc::new(Mutex::new(now)),
        }
    }

    /// Move it forward.
    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
        *now += by;
    }

    /// Jump to a specific instant, forward or back — the only way to test what a
    /// replica does when the wall clock is adjusted underneath it.
    pub fn set(&self, to: OffsetDateTime) {
        let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
        *now = to;
    }

    fn default_start() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(Self::EPOCH).unwrap_or(OffsetDateTime::UNIX_EPOCH)
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TestClock {
    fn now(&self) -> OffsetDateTime {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_clock_stands_still_until_pushed() {
        let clock = TestClock::new();
        let first = clock.now();
        assert_eq!(clock.now(), first);

        clock.advance(Duration::days(30));
        assert_eq!(clock.now(), first + Duration::days(30));
    }

    #[test]
    fn clones_share_the_same_time() {
        // Every port in a test holds its own clone; they must not drift apart.
        let clock = TestClock::new();
        let other = clock.clone();
        clock.advance(Duration::hours(1));
        assert_eq!(other.now(), clock.now());
    }
}
