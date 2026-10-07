//! Delivery and read receipts (§7.3): opt-in per channel and unsigned (ADR-0005).
//! "Seen by 3/5" counts people only; service accounts never move it (§5.2bis, T25).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{DeviceId, NotificationId, ReceiptId};
use crate::principal::{Author, Principal};

/// How far a notification got.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptKind {
    /// It reached a device. Posted automatically.
    Delivered,
    /// A person looked at it. Requires a deliberate act in a client.
    Seen,
}

/// One receipt.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// Identity.
    pub id: ReceiptId,
    /// What it is about.
    pub notification_id: NotificationId,
    /// Who. Only ever a principal's own devices.
    pub principal: Principal,
    /// Which device; absent for a service account's logical subscription (§5.2bis).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceId>,
    /// How far it got.
    pub kind: ReceiptKind,
    /// Server time.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// One person and when they first saw the notification (§7.3, §10.3).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sighting {
    /// Who, with a display name where the reader is entitled to it (§2.4).
    pub author: Author,
    /// When they first saw it, across all their devices.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// The "seen by 3/5" readership: who saw it and when, plus a delivered-device count (§7.3, §10.3).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Readership {
    /// Who saw it, oldest sighting first — reading order, like the timeline.
    pub seen: Vec<Sighting>,
    /// How many people could see it, or `None` when access is group-derived (show "seen by 3").
    pub audience: Option<usize>,
    /// Distinct devices that acknowledged delivery.
    pub delivered_devices: usize,
}

impl Readership {
    /// Fold a notification's receipts into what a reader is shown. Pass `None` as
    /// the audience for channels with group bindings, whose size is unknowable (§14.2);
    /// a known audience is a floor so the count never exceeds it.
    #[must_use]
    pub fn of(receipts: &[Receipt], audience: Option<usize>) -> Self {
        let mut seen: Vec<Sighting> = Vec::new();
        let mut devices: Vec<DeviceId> = Vec::new();

        for receipt in receipts {
            match receipt.kind {
                // Bots are not readers (§5.2bis, T25).
                ReceiptKind::Seen if receipt.principal.is_human() => {
                    match seen
                        .iter_mut()
                        .find(|sighting| sighting.author.principal == receipt.principal)
                    {
                        Some(known) => known.at = known.at.min(receipt.at),
                        None => seen.push(Sighting {
                            author: Author::anonymous(receipt.principal.clone()),
                            at: receipt.at,
                        }),
                    }
                }
                ReceiptKind::Delivered => {
                    if let Some(device) = receipt.device
                        && !devices.contains(&device)
                    {
                        devices.push(device);
                    }
                }
                ReceiptKind::Seen => {}
            }
        }

        seen.sort_by_key(|sighting| sighting.at);
        Self {
            audience: audience.map(|known| known.max(seen.len())),
            seen,
            delivered_devices: devices.len(),
        }
    }

    /// The numerator: distinct people who have read it.
    #[must_use]
    pub fn seen_by(&self) -> usize {
        self.seen.len()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;
    use crate::ids::{ServiceAccountId, UserId};

    fn receipt(principal: Principal, kind: ReceiptKind) -> Receipt {
        at(principal, kind, 1_785_000_000)
    }

    fn at(principal: Principal, kind: ReceiptKind, unix: i64) -> Receipt {
        Receipt {
            id: ReceiptId::generate(),
            notification_id: NotificationId::generate(),
            principal,
            device: Some(DeviceId::generate()),
            kind,
            at: OffsetDateTime::from_unix_timestamp(unix).unwrap(),
        }
    }

    fn user() -> Principal {
        Principal::User {
            id: UserId::generate(),
        }
    }

    #[test]
    fn a_person_on_three_devices_is_still_one_person() {
        let alice = user();
        let receipts = vec![
            receipt(alice.clone(), ReceiptKind::Seen),
            receipt(alice, ReceiptKind::Seen),
        ];
        assert_eq!(Readership::of(&receipts, Some(5)).seen_by(), 1);
    }

    #[test]
    fn delivery_is_not_readership() {
        let receipts = vec![receipt(user(), ReceiptKind::Delivered)];
        assert_eq!(Readership::of(&receipts, Some(5)).seen_by(), 0);
    }

    #[test]
    fn a_bot_reading_a_channel_never_moves_the_count() {
        let receipts = vec![
            receipt(
                Principal::ServiceAccount {
                    id: ServiceAccountId::parse("bot-argocd").unwrap(),
                },
                ReceiptKind::Seen,
            ),
            receipt(user(), ReceiptKind::Seen),
        ];
        let roll = Readership::of(&receipts, Some(5));

        assert_eq!(roll.seen_by(), 1, "\"seen by 3/5\" is about people");
        assert_eq!(roll.audience, Some(5));
    }

    // --- The hover card behind the badge (design §10.3) -------------------

    #[test]
    fn a_person_is_shown_at_the_first_screen_they_read_it_on() {
        let alice = user();
        let roll = Readership::of(
            &[
                at(alice.clone(), ReceiptKind::Seen, 1_785_000_600),
                at(alice, ReceiptKind::Seen, 1_785_000_000),
            ],
            Some(5),
        );

        assert_eq!(roll.seen.len(), 1);
        assert_eq!(
            roll.seen[0].at.unix_timestamp(),
            1_785_000_000,
            "opening it again on a phone is not a second reading"
        );
    }

    #[test]
    fn sightings_read_oldest_first() {
        let roll = Readership::of(
            &[
                at(user(), ReceiptKind::Seen, 1_785_000_600),
                at(user(), ReceiptKind::Seen, 1_785_000_000),
            ],
            Some(5),
        );

        let order: Vec<i64> = roll
            .seen
            .iter()
            .map(|one| one.at.unix_timestamp())
            .collect();
        assert_eq!(order, vec![1_785_000_000, 1_785_000_600]);
    }

    #[test]
    fn delivery_counts_screens_and_only_screens() {
        let device = DeviceId::generate();
        let twice_on_one_device = |principal: Principal| Receipt {
            device: Some(device),
            ..receipt(principal, ReceiptKind::Delivered)
        };
        let no_device = Receipt {
            device: None,
            ..receipt(
                Principal::ServiceAccount {
                    id: ServiceAccountId::parse("bot-argocd").unwrap(),
                },
                ReceiptKind::Delivered,
            )
        };

        let roll = Readership::of(
            &[
                twice_on_one_device(user()),
                twice_on_one_device(user()),
                no_device,
                receipt(user(), ReceiptKind::Delivered),
            ],
            Some(5),
        );

        assert_eq!(
            roll.delivered_devices, 2,
            "one device is one screen however many receipts it posted, \
             and a subscription without a device is not a screen"
        );
        assert_eq!(roll.seen.len(), 0, "delivery is not readership");
    }

    #[test]
    fn an_audience_nobody_can_count_stays_uncounted() {
        // Group-derived access: no denominator from member rows.
        let roll = Readership::of(
            &[
                at(user(), ReceiptKind::Seen, 1_785_000_000),
                at(user(), ReceiptKind::Seen, 1_785_000_100),
            ],
            None,
        );

        assert_eq!(roll.seen_by(), 2);
        assert_eq!(roll.audience, None, "a fraction needs a knowable total");
    }

    #[test]
    fn the_roll_says_on_the_wire_what_the_hover_card_reads() {
        // The field names are wire contract (§14.4).
        let alice = user();
        let roll = Readership::of(&[at(alice, ReceiptKind::Seen, 1_785_000_000)], None);

        let json = serde_json::to_value(&roll).unwrap();

        assert_eq!(json["seen"][0]["at"], "2026-07-25T17:20:00Z");
        assert_eq!(json["seen"][0]["author"]["kind"], "user");
        assert!(
            json["seen"][0]["author"].get("display_name").is_none(),
            "an author with no profile omits the name rather than sending null"
        );
        assert!(
            json["audience"].is_null(),
            "an audience nobody can count is `null`, not a number and not absent"
        );
        assert_eq!(json["delivered_devices"], 0);
    }

    #[test]
    fn a_known_audience_is_a_floor_rather_than_a_ceiling() {
        // Somebody read it and was then removed from the channel.
        let roll = Readership::of(
            &[
                at(user(), ReceiptKind::Seen, 1_785_000_000),
                at(user(), ReceiptKind::Seen, 1_785_000_100),
            ],
            Some(1),
        );

        assert_eq!(
            roll.audience,
            Some(2),
            "\"2/1\" is not a thing anyone can read"
        );
    }
}
