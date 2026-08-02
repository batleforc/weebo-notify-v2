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
use crate::principal::Principal;

/// How far a notification got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptKind {
    /// It reached a device. Posted automatically.
    Delivered,
    /// A person looked at it. Requires a deliberate act in a client.
    Seen,
}

/// One receipt.
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

/// The "seen by 3/5" summary shown to authorised members (design §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenTally {
    /// Distinct people who have seen it.
    pub seen: usize,
    /// People who could.
    pub audience: usize,
}

impl SeenTally {
    /// Count distinct humans who posted a `seen` receipt.
    ///
    /// `audience` is the number of human members of the channel, which the caller
    /// knows and the receipt list does not.
    #[must_use]
    pub fn count(receipts: &[Receipt], audience: usize) -> Self {
        let mut seen: Vec<&Principal> = Vec::new();
        for receipt in receipts {
            let counts = receipt.kind == ReceiptKind::Seen
                && receipt.principal.is_human()
                && !seen.contains(&&receipt.principal);
            if counts {
                seen.push(&receipt.principal);
            }
        }
        Self {
            seen: seen.len(),
            audience,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;
    use crate::ids::{ServiceAccountId, UserId};

    fn receipt(principal: Principal, kind: ReceiptKind) -> Receipt {
        Receipt {
            id: ReceiptId::generate(),
            notification_id: NotificationId::generate(),
            principal,
            device: Some(DeviceId::generate()),
            kind,
            at: OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap(),
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
        assert_eq!(SeenTally::count(&receipts, 5).seen, 1);
    }

    #[test]
    fn delivery_is_not_readership() {
        let receipts = vec![receipt(user(), ReceiptKind::Delivered)];
        assert_eq!(SeenTally::count(&receipts, 5).seen, 0);
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
        assert_eq!(
            SeenTally::count(&receipts, 5),
            SeenTally {
                seen: 1,
                audience: 5
            },
            "\"seen by 3/5\" is a statement about people"
        );
    }
}
