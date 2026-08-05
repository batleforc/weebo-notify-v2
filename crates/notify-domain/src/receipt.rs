//! Delivery and read receipts (design §7.3).
//!
//! Opt-in per channel, and deliberately unsigned. Signing receipts on the device
//! was tried and dropped (ADR-0005): authenticity rests on the device having
//! authenticated to the server, which is already the trust model everywhere else
//! (design §6). A principal may only post receipts for its own devices, and the
//! server — which is trusted — stamps the time. Forging one means owning a
//! device session, which is outside the threat model.
//!
//! The count that reaches users, "seen by 3/5", counts **people**. A service
//! account holding a subscription is not somebody who has been informed
//! (design §5.2bis, scenario T25), so it never moves that number.

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
    /// Which device. Absent for a principal with no device of its own — a service
    /// account holding a logical subscription (design §5.2bis).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceId>,
    /// How far it got.
    pub kind: ReceiptKind,
    /// Server time.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// One person, and when they first looked (design §7.3, §10.3).
///
/// Serialize only, here and on [`Readership`]: a roll is computed and sent, never
/// read back in. Deriving `Deserialize` would describe an input that does not
/// exist, and leave the code that parses it untested for as long as that stays
/// true.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sighting {
    /// Who. Carries the display name where the reader is entitled to it — a hover
    /// card that listed raw identifiers would answer a question nobody asked
    /// (design §2.4).
    pub author: Author,
    /// When they *first* saw it. Somebody who opens a notification on their laptop
    /// and again on their phone read it once, at the first of the two.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// What "seen by 3/5" unfolds into (design §7.3, §10.3).
///
/// The number on the badge is the headline; the hover card is the answer to the
/// question the badge provokes — *who*, and *when*. Delivery stays folded underneath
/// as a device count, because "it reached four screens" and "three people read it"
/// are different facts and only the second one is about readership.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Readership {
    /// Who saw it, oldest sighting first — reading order, like the timeline.
    pub seen: Vec<Sighting>,
    /// How many people could see it, where that is a knowable number.
    ///
    /// `None` on a channel whose access is derived rather than stored — see
    /// [`Readership::of`]. A client shows "vu par 3" then, not "vu par 3/3".
    pub audience: Option<usize>,
    /// Distinct devices that acknowledged delivery. A receipt without a device is
    /// a logical subscription (design §5.2bis), and a subscription is not a screen.
    pub delivered_devices: usize,
}

impl Readership {
    /// Fold a notification's receipts into what a reader is shown.
    ///
    /// # Why the denominator is optional
    ///
    /// A channel grants access two ways (design §11): membership rows, which can be
    /// counted, and bindings against a `groups` claim, which cannot — §5.1 keeps
    /// zero server-side session and §14.2 derives group access per request *on
    /// purpose*, so that leaving a group takes effect at the next call rather than
    /// at the next reconciliation. Nothing therefore knows how many people are in
    /// `infra-admins`.
    ///
    /// Dividing by the member rows that happen to exist would render "vu par 3/3"
    /// on a channel thirty people can read, which is worse than no fraction at all:
    /// it says "everyone has seen this" and it is wrong. So a channel with group
    /// bindings passes `None` and the badge shows a bare count.
    ///
    /// When the number *is* knowable it is still a floor, because a member can be
    /// removed after reading and "4/3" is not something anybody can read.
    #[must_use]
    pub fn of(receipts: &[Receipt], audience: Option<usize>) -> Self {
        let mut seen: Vec<Sighting> = Vec::new();
        let mut devices: Vec<DeviceId> = Vec::new();

        for receipt in receipts {
            match receipt.kind {
                // A bot holding a subscription is not somebody who has been
                // informed (design §5.2bis, scenario T25).
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
        // A channel whose access comes from a `groups` claim (design §14.2): the
        // readers are real, the membership rows are not, and inventing a
        // denominator out of the rows that exist would say "everybody has read
        // this" on a channel thirty people can see.
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
        // The names matter: a client renders this and nothing in the OpenAPI
        // document would catch a rename that still compiles (the D1 lesson, §14.4).
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
