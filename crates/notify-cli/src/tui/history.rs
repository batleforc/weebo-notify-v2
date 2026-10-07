//! The `:` line's persisted history (§9.2): per profile beside the store but not in it (the
//! store may be wiped to re-sync), written 0600 like a shell history.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// How many lines are kept.
const CAPACITY: usize = 200;

/// The `:` line's memory: the lines, and where the caller is inside them.
#[derive(Debug, Default)]
pub(crate) struct History {
    /// Oldest first, so pushing is appending — the same order as the file.
    lines: Vec<String>,
    /// Where an up-arrow walk currently is; `None` means on the line being typed.
    at: Option<usize>,
    /// Where it is written, or `None` without a data directory (history then lives in memory).
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

    /// Remember a line that was just run, and write it down; an immediate repeat isn't added.
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

        // Rewritten whole: at most `CAPACITY` short lines, and that keeps the cap honest.
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

    /// The next line, walking forwards; `None` once back on the line being typed (clear it).
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
        // On creation only; an existing file keeps its owner's mode.
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

        let path = temp_path("perms");
        let mut history = History::load(Some(path.clone()));
        history.push("publish -c ci-infra -t 'deploy 42'");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();

        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[test]
    fn a_front_end_with_nowhere_to_write_still_remembers_while_it_runs() {
        let mut history = History::load(None);
        history.push("ls");

        assert_eq!(history.previous(), Some("ls"));
    }

    #[test]
    fn the_history_sits_beside_the_store_rather_than_inside_it() {
        let path = history_path("work");

        assert_eq!(path.file_name().unwrap(), "history");
        assert_eq!(
            path.parent(),
            notify_client_core::store_path("work").parent()
        );
    }
}
