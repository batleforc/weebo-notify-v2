//! Retention (§7.2bis): lifecycle notifications start ageing when they close,
//! simple ones at creation. Requests above a channel's ceiling are clamped and
//! reported. [`CloseAfter`] closes idle lifecycle notifications so their TTL can start.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;

use crate::duration::HumanDuration;
use crate::error::{DomainError, Result};

/// How long something is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    /// Kept until deleted by hand; written `"0"`, only from channels that opted in (§7.2bis).
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
    /// Rendered as a duration string, `"0s"` meaning forever.
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

// Hand-written: an enum in Rust, a plain string on the wire.
#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for Ttl {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .description(Some(
                "How long to keep it: a single-unit duration such as `48h`, or `0` for forever.",
            ))
            .examples([serde_json::json!("7d")])
            .into()
    }
}

#[cfg(feature = "openapi")]
impl utoipa::ToSchema for Ttl {}

/// How long a lifecycle notification may sit idle before the server closes it;
/// spelled like [`Ttl`] (`"0"` = never).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAfter {
    /// The server never closes on its own; written `"0"`, needs no opt-in.
    Never,
    /// Closed once nothing has happened for this long.
    After(HumanDuration),
}

impl CloseAfter {
    /// Read an operator-supplied value. `"0"` means never.
    pub fn parse(raw: &str, field: &'static str) -> Result<Self> {
        let duration = HumanDuration::parse(raw, field)?;
        Ok(Self::from_duration(duration))
    }

    /// Interpret a parsed duration, mapping zero to [`CloseAfter::Never`].
    #[must_use]
    pub fn from_duration(duration: HumanDuration) -> Self {
        if duration.is_zero() {
            Self::Never
        } else {
            Self::After(duration)
        }
    }

    /// The duration, when there is one.
    #[must_use]
    pub fn duration(self) -> Option<HumanDuration> {
        match self {
            Self::Never => None,
            Self::After(duration) => Some(duration),
        }
    }

    /// When a notification last active at `last_activity` becomes due for closure;
    /// `None` for [`CloseAfter::Never`] or on calendar overflow.
    #[must_use]
    pub fn due_at(self, last_activity: OffsetDateTime) -> Option<OffsetDateTime> {
        let duration = self.duration()?;
        last_activity.checked_add(time::Duration::seconds(
            i64::try_from(duration.as_secs()).ok()?,
        ))
    }
}

impl Default for CloseAfter {
    /// One week, matching the channel's default TTL (§7.2bis).
    fn default() -> Self {
        Self::After(HumanDuration::from_secs_const(7 * 24 * 3_600))
    }
}

impl Serialize for CloseAfter {
    /// A duration string, `"0s"` meaning never.
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        match self {
            Self::Never => HumanDuration::ZERO.serialize(serializer),
            Self::After(duration) => duration.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for CloseAfter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        HumanDuration::deserialize(deserializer).map(Self::from_duration)
    }
}

// Hand-written like [`Ttl`]'s: an enum here, a string on the wire.
#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for CloseAfter {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .description(Some(
                "How long a lifecycle notification may go without activity before the server \
                 closes it: a single-unit duration such as `7d`, or `0` to never close it \
                 automatically.",
            ))
            .examples([serde_json::json!("7d")])
            .into()
    }
}

#[cfg(feature = "openapi")]
impl utoipa::ToSchema for CloseAfter {}

/// What retention a channel allows (§7.2bis, configured per channel in §11).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtlPolicy {
    /// Applied when the publisher says nothing.
    pub default: Ttl,
    /// Forced ceiling. A longer request is clamped down to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<HumanDuration>,
    /// Whether `"0"` (keep forever) is accepted; off by default.
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtlResolution {
    /// What will actually be applied.
    pub ttl: Ttl,
    /// Set when the request exceeded the channel ceiling, so the clamp is visible (T6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clamped_from: Option<HumanDuration>,
}

impl TtlPolicy {
    /// Resolve a publisher request against this policy; `None` gets the (also
    /// clamped) channel default.
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
                    // The default contradicts the policy: use the ceiling, or refuse.
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
    use time::Duration as TimeDuration;

    use super::CloseAfter::Never;
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
    fn close_after_defaults_to_a_week_and_zero_opts_out() {
        assert_eq!(
            CloseAfter::default().duration().map(HumanDuration::as_secs),
            Some(7 * 24 * 3_600),
            "a channel that says nothing should behave the way somebody would \
             describe it without reading any configuration"
        );
        assert_eq!(CloseAfter::parse("0", "close_after").unwrap(), Never);
        assert_eq!(Never.duration(), None);
    }

    /// The whole point of the type: a deadline the sweep can compare against.
    #[test]
    fn close_after_turns_activity_into_a_deadline() {
        let last = OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap();

        assert_eq!(
            CloseAfter::After(hours(48)).due_at(last),
            Some(last + TimeDuration::hours(48))
        );
        assert_eq!(
            Never.due_at(last),
            None,
            "a channel that opted out must produce no deadline at all, not a \
             deadline far away — the sweep asks this question to build its query"
        );
    }

    /// `"0"` round-trips through the stored JSON policy.
    #[test]
    fn close_after_round_trips_through_its_wire_form() {
        for value in [Never, CloseAfter::After(hours(48)), CloseAfter::default()] {
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(serde_json::from_str::<CloseAfter>(&json).unwrap(), value);
        }
        assert_eq!(serde_json::to_string(&Never).unwrap(), "\"0s\"");
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
