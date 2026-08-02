//! Time as a port.
//!
//! The domain never reads a clock — it is told what time it is (see
//! `notify-domain`). This is the seam where that promise is kept: production wires
//! [`SystemClock`], tests wire a clock they control, and a scenario about a TTL
//! expiring in thirty days runs in a microsecond instead of being skipped.

use time::OffsetDateTime;

/// The current time.
///
/// Not async and not fallible: reading the clock is neither. Making it a trait is
/// about *control*, not about I/O.
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
