//! What GitOps last applied, so a pass can journal what changed (§11, §13).
//! Diffs the declaration, not the live row, which managers may legitimately edit.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::PortResult;

/// Which family of declaration a snapshot belongs to (closed: stored and read back).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DeclarationKind {
    /// A `[[trust_policy]]` block — which forge may publish here (§5.3).
    TrustPolicy,
    /// A `[[channels.managed]]` block.
    Channel,
    /// A `[[service_accounts]]` block.
    ServiceAccount,
}

impl DeclarationKind {
    /// The stored form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TrustPolicy => "trust_policy",
            Self::Channel => "channel",
            Self::ServiceAccount => "service_account",
        }
    }
}

impl core::fmt::Display for DeclarationKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One declaration as it was applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    /// Identity within its kind: a channel id, an account id, a policy's issuer.
    pub key: String,
    /// The declaration itself, canonicalised by the writer and compared whole.
    pub snapshot: serde_json::Value,
}

/// What the reconciler last wrote, per kind.
#[async_trait::async_trait]
pub trait DeclarationStore: Send + Sync {
    /// Everything of one kind (the whole set, so removals are visible).
    async fn snapshot(&self, kind: DeclarationKind) -> PortResult<Vec<Declaration>>;

    /// Replace the stored set for one kind with what was just applied (not an upsert,
    /// so removals are forgotten).
    async fn apply(
        &self,
        kind: DeclarationKind,
        declarations: &[Declaration],
        at: OffsetDateTime,
    ) -> PortResult<()>;
}

/// What one pass changed, per kind (§13), as computed by [`diff`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// Declared now, absent before.
    pub added: Vec<Declaration>,
    /// Declared before and now, with a different body.
    pub changed: Vec<Changed>,
    /// Declared before, gone now.
    pub removed: Vec<Declaration>,
}

/// A declaration that moved, with both sides so the audit entry is self-contained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    /// What was applied before.
    pub previous: Declaration,
    /// What is applied now.
    pub current: Declaration,
}

impl Changes {
    /// Whether anything moved; a steady instance must write nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

/// Compare what was applied before with what is applied now, ordered by key so
/// the audit log is stable across runs.
#[must_use]
pub fn diff(before: &[Declaration], now: &[Declaration]) -> Changes {
    use std::collections::BTreeMap;

    let previous: BTreeMap<&str, &Declaration> = before
        .iter()
        .map(|declaration| (declaration.key.as_str(), declaration))
        .collect();
    let current: BTreeMap<&str, &Declaration> = now
        .iter()
        .map(|declaration| (declaration.key.as_str(), declaration))
        .collect();

    let mut changes = Changes::default();

    for (key, declaration) in &current {
        match previous.get(key) {
            None => changes.added.push((*declaration).clone()),
            Some(was) if was.snapshot != declaration.snapshot => {
                changes.changed.push(Changed {
                    previous: (*was).clone(),
                    current: (*declaration).clone(),
                });
            }
            Some(_) => {}
        }
    }

    for (key, declaration) in &previous {
        if !current.contains_key(key) {
            changes.removed.push((*declaration).clone());
        }
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(key: &str, body: &str) -> Declaration {
        Declaration {
            key: key.to_owned(),
            snapshot: serde_json::json!({ "body": body }),
        }
    }

    #[test]
    fn an_unchanged_file_produces_nothing() {
        let before = vec![declaration("a", "one"), declaration("b", "two")];

        assert!(diff(&before, &before.clone()).is_empty());
    }

    #[test]
    fn added_changed_and_removed_are_three_different_answers() {
        let before = vec![declaration("kept", "same"), declaration("gone", "x")];
        let now = vec![declaration("kept", "edited"), declaration("fresh", "y")];

        let changes = diff(&before, &now);

        assert_eq!(changes.added, vec![declaration("fresh", "y")]);
        assert_eq!(
            changes.changed,
            vec![Changed {
                previous: declaration("kept", "same"),
                current: declaration("kept", "edited"),
            }]
        );
        assert_eq!(changes.removed, vec![declaration("gone", "x")]);
    }

    #[test]
    fn a_first_pass_against_an_empty_store_is_all_additions() {
        let now = vec![declaration("a", "one"), declaration("b", "two")];

        let changes = diff(&[], &now);

        assert_eq!(changes.added.len(), 2);
        assert!(changes.changed.is_empty() && changes.removed.is_empty());
    }

    #[test]
    fn changes_come_out_in_a_stable_order() {
        let before = vec![declaration("b", "x"), declaration("a", "x")];
        let now = vec![declaration("b", "y"), declaration("a", "y")];

        let changes = diff(&before, &now);

        assert_eq!(
            changes
                .changed
                .iter()
                .map(|changed| changed.current.key.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }
}
