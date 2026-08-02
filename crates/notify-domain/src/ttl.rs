//! Retention: how long a notification stays, and from when (design §7.2bis).
//!
//! The subtle part is not the duration, it is the **starting point**, and it
//! differs by notification type:
//!
//! * A **lifecycle** notification (published with a `state`) does not start
//!   ageing until it closes. A long incident stays visible until it is resolved,
//!   however long that takes, and reopening it suspends the clock again.
//! * A **simple** notification (published without a `state`) is born closed, so
//!   its clock starts at creation.
//!
//! The channel owns the policy: a default, an optional forced ceiling, and whether
//! infinite retention is allowed at all. A publisher asking for more than the
//! ceiling is **clamped and told**, not refused — a CI job should not fail because
//! it wanted its build log kept a week longer than the operator allows.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::duration::HumanDuration;
use crate::error::{DomainError, Result};

/// How long something is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    /// Kept until deleted by hand. Written `"0"` on the wire, and only accepted
    /// from a channel that opted in (design §7.2bis).
    Forever,
    /// Kept for this long past the moment the clock starts.
    For(HumanDuration),
}

impl Ttl {
    /// Read a publisher- or operator-supplied value. `"0"` means forever.
    pub fn parse(raw: &str, field: &'static str) -> Result<Self> {
        let duration = HumanDuration::parse(raw, field)?;
        Ok(Self::from_duration(duration))
    }

    /// Interpret a parsed duration, mapping zero to [`Ttl::Forever`].
    #[must_use]
    pub fn from_duration(duration: HumanDuration) -> Self {
        if duration.is_zero() {
            Self::Forever
        } else {
            Self::For(duration)
        }
    }

    /// The duration, when there is one.
    #[must_use]
    pub fn duration(self) -> Option<HumanDuration> {
        match self {
            Self::Forever => None,
            Self::For(duration) => Some(duration),
        }
    }
}

impl Serialize for Ttl {
    /// Rendered as a duration string, with `"0s"` standing for forever — the same
    /// spelling a publisher writes.
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        match self {
            Self::Forever => HumanDuration::ZERO.serialize(serializer),
            Self::For(duration) => duration.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Ttl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        HumanDuration::deserialize(deserializer).map(Self::from_duration)
    }
}

/// What a channel allows (design §7.2bis, configured per channel in §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtlPolicy {
    /// Applied when the publisher says nothing.
    pub default: Ttl,
    /// Forced ceiling. A longer request is clamped down to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<HumanDuration>,
    /// Whether `"0"` is accepted. Off by default: unbounded retention is a
    /// deliberate operator decision, not something a publisher can opt into.
    pub allow_forever: bool,
}

impl Default for TtlPolicy {
    fn default() -> Self {
        Self {
            // The channel default named in the design (§7.2bis).
            default: Ttl::For(HumanDuration::from_secs_const(7 * 24 * 3_600)),
            max: None,
            allow_forever: false,
        }
    }
}

/// The outcome of applying a [`TtlPolicy`] to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtlResolution {
    /// What will actually be applied.
    pub ttl: Ttl,
    /// Set when the request was longer than the channel ceiling. Reported back to
    /// the publisher so a clamp is visible rather than silent (scenario T6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clamped_from: Option<HumanDuration>,
}

impl TtlPolicy {
    /// Resolve a publisher request against this policy.
    ///
    /// `None` means the publisher did not ask, and gets the channel default —
    /// itself clamped, so a policy tightened after the fact takes effect on
    /// everything rather than only on explicit requests.
    pub fn resolve(&self, requested: Option<Ttl>) -> Result<TtlResolution> {
        let asked = requested.unwrap_or(self.default);

        match asked {
            Ttl::Forever => {
                if self.allow_forever {
                    Ok(TtlResolution {
                        ttl: Ttl::Forever,
                        clamped_from: None,
                    })
                } else if requested.is_none() {
                    // The channel's own default contradicts its own policy. Trust
                    // the ceiling if there is one, and refuse otherwise rather than
                    // silently granting unbounded retention.
                    self.max
                        .map_or(Err(DomainError::InfiniteTtlNotAllowed), |max| {
                            Ok(TtlResolution {
                                ttl: Ttl::For(max),
                                clamped_from: None,
                            })
                        })
                } else {
                    Err(DomainError::InfiniteTtlNotAllowed)
                }
            }
            Ttl::For(duration) => match self.max {
                Some(max) if duration > max => Ok(TtlResolution {
                    ttl: Ttl::For(max),
                    clamped_from: Some(duration),
                }),
                _ => Ok(TtlResolution {
                    ttl: Ttl::For(duration),
                    clamped_from: None,
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    fn hours(n: u64) -> HumanDuration {
        HumanDuration::from_secs(n * 3_600, "ttl").unwrap()
    }

    #[test]
    fn zero_means_forever_and_needs_an_opt_in() {
        assert_eq!(Ttl::parse("0", "ttl").unwrap(), Ttl::Forever);

        let strict = TtlPolicy::default();
        assert_eq!(
            strict.resolve(Some(Ttl::Forever)).unwrap_err(),
            DomainError::InfiniteTtlNotAllowed
        );

        let permissive = TtlPolicy {
            allow_forever: true,
            ..TtlPolicy::default()
        };
        assert_eq!(
            permissive.resolve(Some(Ttl::Forever)).unwrap().ttl,
            Ttl::Forever
        );
    }

    #[test]
    fn a_request_past_the_ceiling_is_clamped_and_reported() {
        let policy = TtlPolicy {
            max: Some(hours(24)),
            ..TtlPolicy::default()
        };

        let resolved = policy.resolve(Some(Ttl::For(hours(72)))).unwrap();
        assert_eq!(resolved.ttl, Ttl::For(hours(24)));
        assert_eq!(
            resolved.clamped_from,
            Some(hours(72)),
            "a clamp the publisher cannot see is a clamp they will report as a bug"
        );

        let resolved = policy.resolve(Some(Ttl::For(hours(1)))).unwrap();
        assert_eq!(resolved.clamped_from, None);
    }

    #[test]
    fn no_request_takes_the_channel_default() {
        let policy = TtlPolicy {
            default: Ttl::For(hours(2)),
            ..TtlPolicy::default()
        };
        assert_eq!(policy.resolve(None).unwrap().ttl, Ttl::For(hours(2)));
    }

    #[test]
    fn a_default_of_forever_never_outranks_a_ceiling() {
        // Contradictory config: the safe reading is the ceiling, not unbounded.
        let policy = TtlPolicy {
            default: Ttl::Forever,
            max: Some(hours(24)),
            allow_forever: false,
        };
        assert_eq!(policy.resolve(None).unwrap().ttl, Ttl::For(hours(24)));
    }
}
