//! Silencing: the reactive, targeted half of "stop notifying me" (design §2.3).
//!
//! Do-not-disturb is the other half — planned, broad, and arriving with M3
//! (design §7.4). Both are per user and both live on the server, so muting on one
//! client mutes everywhere rather than only where it was clicked.
//!
//! A silence suppresses **push and badges** for one user. It does not touch the
//! inbox, the stream, or anybody else's delivery: the notification still arrives,
//! it just stops asking for attention.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::action::SilenceScope;
use crate::duration::HumanDuration;
use crate::error::{DomainError, Result};
use crate::ids::{NotificationId, UserId};

/// What a silence covers.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum SilenceTarget {
    /// One notification.
    Notification {
        /// The notification silenced.
        id: NotificationId,
    },
    /// Everything sharing a `group_key` (design §2.3).
    Group {
        /// The group key silenced.
        key: String,
    },
}

/// An active silence, held per user.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Silence {
    /// Whose it is. Silences never leak across users (scenario T20).
    pub user: UserId,
    /// What it covers.
    pub target: SilenceTarget,
    /// When it lifts. `None` only for a notification-scoped silence, which ends
    /// when the notification does.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub until: Option<OffsetDateTime>,
    /// When it was set.
    #[serde(with = "time::serde::rfc3339")]
    pub since: OffsetDateTime,
}

impl Silence {
    /// Start a silence.
    ///
    /// A group silence with no duration is refused: muting a whole group with no
    /// end date is the kind of thing someone does at 3am and rediscovers a quarter
    /// later (design §2.3). Scoped to a single notification, "until it closes" is a
    /// perfectly good answer, so no duration is required there.
    pub fn start(
        user: UserId,
        target: SilenceTarget,
        duration: Option<HumanDuration>,
        now: OffsetDateTime,
    ) -> Result<Self> {
        let until = match (&target, duration) {
            (SilenceTarget::Group { .. }, None) => {
                return Err(DomainError::SilenceGroupNeedsDuration);
            }
            (_, Some(duration)) => Some(
                now.checked_add(time::Duration::seconds(
                    i64::try_from(duration.as_secs()).map_err(|_| DomainError::Malformed {
                        field: "silence.duration",
                        reason: "is not a representable instant",
                    })?,
                ))
                .ok_or(DomainError::Malformed {
                    field: "silence.duration",
                    reason: "is not a representable instant",
                })?,
            ),
            (SilenceTarget::Notification { .. }, None) => None,
        };

        Ok(Self {
            user,
            target,
            until,
            since: now,
        })
    }

    /// Whether it is still in force.
    #[must_use]
    pub fn is_active(&self, now: OffsetDateTime) -> bool {
        self.until.is_none_or(|until| now < until)
    }

    /// Whether it covers a given notification, identified by its id and its
    /// `group_key`.
    #[must_use]
    pub fn covers(&self, notification: NotificationId, group_key: Option<&str>) -> bool {
        match &self.target {
            SilenceTarget::Notification { id } => *id == notification,
            SilenceTarget::Group { key } => group_key == Some(key.as_str()),
        }
    }
}

/// The scope an action asked for, resolved into a concrete target.
///
/// A group-scoped `silence` button on a notification with no `group_key` has
/// nothing to act on; rather than silently silencing only that notification —
/// which would look like the button half-worked — it is refused.
pub fn target_for(
    scope: SilenceScope,
    notification: NotificationId,
    group_key: Option<&str>,
) -> Result<SilenceTarget> {
    match scope {
        SilenceScope::Notification => Ok(SilenceTarget::Notification { id: notification }),
        SilenceScope::Group => match group_key {
            Some(key) => Ok(SilenceTarget::Group {
                key: key.to_owned(),
            }),
            None => Err(DomainError::Malformed {
                field: "silence.scope",
                reason: "this notification has no `group_key` to silence",
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use time::Duration as TimeDuration;

    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap()
    }

    fn four_hours() -> HumanDuration {
        HumanDuration::from_secs_const(4 * 3_600)
    }

    #[test]
    fn a_group_silence_must_be_time_boxed() {
        let refused = Silence::start(
            UserId::generate(),
            SilenceTarget::Group {
                key: "incident-4521".to_owned(),
            },
            None,
            now(),
        );
        assert_eq!(refused.unwrap_err(), DomainError::SilenceGroupNeedsDuration);
    }

    #[test]
    fn a_notification_silence_may_simply_last_until_it_closes() {
        let silence = Silence::start(
            UserId::generate(),
            SilenceTarget::Notification {
                id: NotificationId::generate(),
            },
            None,
            now(),
        )
        .unwrap();
        assert_eq!(silence.until, None);
        assert!(silence.is_active(now() + TimeDuration::days(365)));
    }

    #[test]
    fn a_timed_silence_lifts_on_its_own() {
        let silence = Silence::start(
            UserId::generate(),
            SilenceTarget::Group {
                key: "incident-4521".to_owned(),
            },
            Some(four_hours()),
            now(),
        )
        .unwrap();

        assert!(silence.is_active(now() + TimeDuration::hours(3)));
        assert!(!silence.is_active(now() + TimeDuration::hours(4)));
    }

    #[test]
    fn a_group_silence_covers_everything_sharing_the_key() {
        let silence = Silence::start(
            UserId::generate(),
            SilenceTarget::Group {
                key: "incident-4521".to_owned(),
            },
            Some(four_hours()),
            now(),
        )
        .unwrap();

        assert!(silence.covers(NotificationId::generate(), Some("incident-4521")));
        assert!(!silence.covers(NotificationId::generate(), Some("incident-9999")));
        assert!(!silence.covers(NotificationId::generate(), None));
    }

    #[test]
    fn a_notification_silence_covers_only_that_notification() {
        let id = NotificationId::generate();
        let silence = Silence::start(
            UserId::generate(),
            SilenceTarget::Notification { id },
            None,
            now(),
        )
        .unwrap();

        assert!(silence.covers(id, Some("incident-4521")));
        assert!(!silence.covers(NotificationId::generate(), Some("incident-4521")));
    }

    #[test]
    fn a_group_button_on_an_ungrouped_notification_is_refused_not_downgraded() {
        let id = NotificationId::generate();
        assert!(target_for(SilenceScope::Group, id, None).is_err());
        assert!(target_for(SilenceScope::Notification, id, None).is_ok());
    }
}
