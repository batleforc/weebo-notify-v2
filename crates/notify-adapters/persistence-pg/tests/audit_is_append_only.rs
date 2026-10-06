//! The audit log is append-only (SEC-P2-37): no SQL in this crate may name `audit_entries` in a
//! destructive statement, and the TTL purge never reaches it. A source scan, so it runs everywhere.

#![allow(
    clippy::expect_used,
    reason = "a failing expect in a test is a failing test, and the message is the diagnosis"
)]

use std::path::Path;

/// Statements that would remove or rewrite history.
const DESTRUCTIVE: &[&str] = &["delete from", "truncate", "drop table", "update"];

/// The table the audit log lives in (`20260803000000_initial.sql`).
const TABLE: &str = "audit_entries";

#[test]
fn no_sql_in_this_crate_destroys_audit_entries() {
    let mut offenders = Vec::new();

    for file in rust_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path()) {
        let text = std::fs::read_to_string(&file).expect("a source file this crate compiles");
        for (number, line) in text.lines().enumerate() {
            let lowered = line.to_ascii_lowercase();
            if !lowered.contains(TABLE) {
                continue;
            }
            if let Some(verb) = DESTRUCTIVE.iter().find(|verb| lowered.contains(**verb)) {
                offenders.push(format!(
                    "{}:{}: `{verb}` against {TABLE} — {}",
                    file.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "the audit log is append-only (advisory SEC-P2-37). If this is a deliberate \
         change, the decision and its retention story belong in an ADR and in \
         hardening.mdx before this test does:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_ttl_purge_only_reclaims_notifications() {
    // The purge is the timed delete most likely to grow a second target.
    let notifications =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/notifications.rs"))
            .expect("the notification store");

    let purge = notifications
        .split("async fn purge(")
        .nth(1)
        .expect("`purge` is the TTL sweep this test exists for");
    // Up to the end of that function's statement, which is enough to cover its SQL.
    let body: String = purge
        .chars()
        .take(1_200)
        .collect::<String>()
        .to_ascii_lowercase();

    assert!(
        body.contains("delete from notifications"),
        "purge no longer deletes notifications; this test is asserting the wrong thing"
    );
    assert!(
        !body.contains(TABLE),
        "the TTL purge now names {TABLE}: audit entries must outlive the \
         notifications they describe, or an incident review reads a log with the \
         interesting rows already reclaimed"
    );
}

/// Every `.rs` under `dir`.
fn rust_sources(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];

    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }

    assert!(
        !found.is_empty(),
        "found no sources to scan — the path is wrong"
    );
    found
}
