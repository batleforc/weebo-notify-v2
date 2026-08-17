//! What the `:` line remembers between runs (design §9.2, §9.3bis).
//!
//! §9.2 asks for a persisted history, and the reason it is persisted rather than
//! kept in the `App` is the same reason the store is a file: a TUI is opened and
//! closed all day, and a history that dies with the process is a history nobody
//! learns to reach for.
//!
//! # Where it lives, and what that costs
//!
//! Beside the store, in the profile's data directory (§9.3bis) — so two profiles
//! keep two histories, exactly as they keep two stores and two devices. It is
//! written `0600` on Unix for the reason a shell history is: a `:publish` line can
//! carry a channel name, a run id, a title, and none of that is other users'
//! business on a shared machine.
//!
//! It is **not** in the store's SQLite: the store is a cache that §9.3bis says may
//! be deleted at any moment to force a re-sync, and losing the history to a cache
//! wipe would be a surprise nobody asked for.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// How many lines are kept. Enough to hold a working session, small enough that
/// the file is read in one gulp at start-up.
const CAPACITY: usize = 200;

/// The `:` line's memory: the lines, and where the caller is inside them.
#[derive(Debug, Default)]
pub(crate) struct History {
    /// Oldest first, so pushing is appending — the same order as the file.
    lines: Vec<String>,
    /// Where an up-arrow walk currently is. `None` means "on the empty line the
    /// caller is typing", which is where a fresh walk starts from.
    at: Option<usize>,
    /// Where it is written, or `None` when this profile has no data directory —
    /// an ephemeral `notify watch` out of a debug pod (§9.3bis) still gets a
    /// history for as long as it runs.
    path: Option<PathBuf>,
}

impl History {
    /// Read whatever the profile remembers.
    pub(crate) fn load(path: Option<PathBuf>) -> Self {
        let lines = path
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| {
                text.lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Self {
            lines,
            at: None,
            path,
        }
    }

    /// Remember a line that was just run, and write it down.
    ///
    /// Repeating the previous line does not grow the file: a `:sync` pressed four
    /// times in a row is one thing somebody did, not four.
    pub(crate) fn push(&mut self, line: &str) {
        self.at = None;

        let line = line.trim();
        if line.is_empty() || self.lines.last().map(String::as_str) == Some(line) {
            return;
        }

        self.lines.push(line.to_owned());
        if self.lines.len() > CAPACITY {
            let excess = self.lines.len() - CAPACITY;
            self.lines.drain(..excess);
        }

        // Rewritten whole rather than appended: the file is at most `CAPACITY`
        // short lines, and a rewrite is what keeps the cap honest without a second
        // pass to trim it.
        if let Some(path) = &self.path {
            let _ = write_private(path, &self.lines);
        }
    }

    /// The previous line, walking backwards. `None` when there is nothing older.
    pub(crate) fn previous(&mut self) -> Option<&str> {
        if self.lines.is_empty() {
            return None;
        }

        let next = match self.at {
            None => self.lines.len() - 1,
            Some(0) => 0,
            Some(current) => current - 1,
        };
        self.at = Some(next);
        self.lines.get(next).map(String::as_str)
    }

    /// The next line, walking forwards. `None` once the walk is back on the line
    /// the caller was typing — which is the signal to clear it.
    pub(crate) fn next(&mut self) -> Option<&str> {
        let current = self.at?;
        if current + 1 >= self.lines.len() {
            self.at = None;
            return None;
        }

        self.at = Some(current + 1);
        self.lines.get(current + 1).map(String::as_str)
    }
}

/// Write the file, readable by its owner and nobody else.
fn write_private(path: &Path, lines: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // On creation only — an existing file keeps whatever the owner set, which
        // is the same rule `ssh` applies to its own files.
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    for line in lines {
        writeln!(file, "{line}")?;
    }
    file.flush()
}

/// `…/<profile>/history`, beside the store.
#[must_use]
pub(crate) fn history_path(profile: &str) -> PathBuf {
    notify_client_core::store_path(profile).with_file_name("history")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    fn temp_path(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wnk-history-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("history")
    }

    #[test]
    fn a_line_survives_the_process_that_typed_it() {
        // The whole reason this is a file: the TUI is opened and closed all day.
        let path = temp_path("survives");
        let mut first = History::load(Some(path.clone()));
        first.push(":publish -c ci-infra -t rebuilt");

        let second = History::load(Some(path));

        assert_eq!(second.lines, [":publish -c ci-infra -t rebuilt"]);
    }

    #[test]
    fn repeating_a_line_does_not_repeat_it_in_the_file() {
        let mut history = History::load(None);
        history.push("sync");
        history.push("sync");
        history.push("ls");
        history.push("sync");

        assert_eq!(history.lines, ["sync", "ls", "sync"]);
    }

    #[test]
    fn walking_back_and_forward_lands_where_it_started() {
        let mut history = History::load(None);
        history.push("one");
        history.push("two");

        assert_eq!(history.previous(), Some("two"), "newest first");
        assert_eq!(history.previous(), Some("one"));
        assert_eq!(history.previous(), Some("one"), "the oldest is a floor");
        assert_eq!(history.next(), Some("two"));
        assert_eq!(
            history.next(),
            None,
            "past the newest is the line being typed, which the caller clears"
        );
    }

    #[test]
    fn running_something_new_restarts_the_walk() {
        // Otherwise the next up-arrow resumes in the middle of yesterday.
        let mut history = History::load(None);
        history.push("one");
        history.push("two");
        assert_eq!(history.previous(), Some("two"));

        history.push("three");

        assert_eq!(history.previous(), Some("three"));
    }

    #[test]
    fn the_file_keeps_the_last_lines_and_not_the_first() {
        let path = temp_path("capacity");
        let mut history = History::load(Some(path.clone()));
        for index in 0..CAPACITY + 10 {
            history.push(&format!("line-{index}"));
        }

        let reloaded = History::load(Some(path));

        assert_eq!(reloaded.lines.len(), CAPACITY);
        assert_eq!(
            reloaded.lines[0], "line-10",
            "the oldest fell off the front"
        );
        assert_eq!(
            reloaded.lines[CAPACITY - 1],
            format!("line-{}", CAPACITY + 9)
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_by_its_owner_and_nobody_else() {
        use std::os::unix::fs::PermissionsExt as _;

        // A `:publish` line carries a channel, a run id, a title. Same rule as a
        // shell history, and as the token fallback of §9.1.
        let path = temp_path("perms");
        let mut history = History::load(Some(path.clone()));
        history.push("publish -c ci-infra -t 'deploy 42'");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();

        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[test]
    fn a_front_end_with_nowhere_to_write_still_remembers_while_it_runs() {
        // The ephemeral `notify watch` of §9.3bis: no store, no device, no file —
        // and no reason to lose the last line typed.
        let mut history = History::load(None);
        history.push("ls");

        assert_eq!(history.previous(), Some("ls"));
    }

    #[test]
    fn the_history_sits_beside_the_store_rather_than_inside_it() {
        // Not in the SQLite: §9.3bis says the store may be deleted at any moment to
        // force a re-sync, and a history lost to a cache wipe is a surprise.
        let path = history_path("work");

        assert_eq!(path.file_name().unwrap(), "history");
        assert_eq!(
            path.parent(),
            notify_client_core::store_path("work").parent()
        );
    }
}
