//! Time as a port: the domain is told the time, tests wire a clock they control.

use time::OffsetDateTime;

/// The current time. A trait for control, not I/O.
pub trait Clock: Send + Sync + core::fmt::Debug {
    /// Now, in UTC.
    fn now(&self) -> OffsetDateTime;
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_clock_moves_forward() {
        let clock = SystemClock;
        assert!(clock.now() <= clock.now());
    }
}
