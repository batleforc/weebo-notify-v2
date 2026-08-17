//! What GitOps last applied, so a pass can say what changed (design §11, §13).
//!
//! # Why previous state has to be stored at all
//!
//! The reconciler applies the whole configuration every pass. It therefore knows
//! what it is applying and never what *moved* — which is why §13's promise that
//! trust policy changes are journalled had no emitter (D28). Channels and service
//! accounts could in principle have been diffed against their live rows; trust
//! policies could not, since they are read into memory at boot and stored nowhere.
//!
//! # Why the declaration and not the entity
//!
//! A managed channel's live row drifts for reasons that are not the file: a manager
//! edits what they are allowed to edit. Diffing against that would report a human's
//! action as a GitOps change and, worse, would report it on every pass until
//! somebody reverted it. What §13 asks is whether the *declaration* changed, so the
//! declaration is what is kept.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::PortResult;

/// Which family of declaration a snapshot belongs to.
///
/// A closed set for the same reason [`crate::AuditAction`] is one: these strings
/// reach a database and come back, and a value that can be written but not read is
/// how `GET /admin/audit` broke once already (M4duovicies).
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
    /// The declaration itself, canonicalised by whoever writes it.
    ///
    /// Compared whole. *What* changed inside it belongs in the audit entry's
    /// `detail`, where a person reads it, and not here, where only equality matters.
    pub snapshot: serde_json::Value,
}

/// What the reconciler last wrote, per kind.
#[async_trait::async_trait]
pub trait DeclarationStore: Send + Sync {
    /// Everything of one kind.
    ///
    /// The whole set and never one key: a pass has to see the declarations that are
    /// about to disappear, and a lookup can only return the ones that remain.
    async fn snapshot(&self, kind: DeclarationKind) -> PortResult<Vec<Declaration>>;

    /// Replace the stored set for one kind with what was just applied.
    ///
    /// Replace rather than upsert, because removals are half of what this answers:
    /// an upsert-only store would remember a trust policy that was withdrawn and go
    /// on reporting it as present for ever.
    async fn apply(
        &self,
        kind: DeclarationKind,
        declarations: &[Declaration],
        at: OffsetDateTime,
    ) -> PortResult<()>;
}

/// What one pass changed, per kind (design §13).
///
/// Built by [`diff`], which is where the comparison lives so that both the real
/// store and the double answer the same question the same way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// Declared now, absent before.
    pub added: Vec<Declaration>,
    /// Declared before and now, with a different body.
    pub changed: Vec<Changed>,
    /// Declared before, gone now.
    pub removed: Vec<Declaration>,
}

/// A declaration that moved, and what it moved from.
///
/// Both sides, because "a trust policy changed" is not the sentence §13 promises —
/// an operator at 3am needs to know *what* it changed to, and reconstructing that
/// from the previous audit entry assumes the previous entry exists, which is false
/// on the pass right after this table was introduced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    /// What was applied before.
    pub previous: Declaration,
    /// What is applied now.
    pub current: Declaration,
}

impl Changes {
    /// Whether the file moved at all.
    ///
    /// A steady instance reconciling every sixty seconds must write **nothing**: an
    /// audit log that gains a row per minute is one nobody reads after an incident,
    /// which would undo the point of filling it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

/// Compare what was applied before with what is being applied now.
///
/// Ordered by key so that a pass which changes three policies writes three entries
/// in a stable order, rather than in whatever order a hash map yielded — an audit
/// log that reorders itself between runs is one that cannot be diffed.
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

        // The property that keeps this log readable: the reconciler runs every
        // sixty seconds, and a row per pass would bury the rows that matter.
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
        // What a fresh deployment writes: every declaration is new. Deliberately
        // not special-cased — "the file arrived" is genuinely a set of additions,
        // and an operator reading the log wants to see what it arrived with.
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

        // Sorted by key, not by declaration order: two passes over the same file
        // must produce the same log, or the log cannot be diffed between runs.
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
