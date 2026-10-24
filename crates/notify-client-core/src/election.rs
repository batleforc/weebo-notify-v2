//! Which front end holds the stream (§9.3bis): whoever holds an exclusive, non-blocking
//! advisory lock beside the per-profile store. A crashed holder releases it by exiting,
//! so there is no lease or heartbeat; losers read the store instead.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::error::ClientError;

/// Proof that this process holds the stream for a profile; dropping it releases the lock.
#[derive(Debug)]
pub struct StreamLease {
    /// Held for its `Drop`: closing the descriptor is the release.
    _file: File,
    path: PathBuf,
}

impl StreamLease {
    /// Where the lock file for this store lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Try to become the front end that holds the stream; `None` when somebody else does.
///
/// # Errors
///
/// Fails only if the lock file cannot be created or opened (unwritable data directory).
pub fn try_hold_stream(store: &Path) -> Result<Option<StreamLease>, ClientError> {
    let path = lock_path(store);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| ClientError::Store {
            detail: format!("cannot create {}: {error}", parent.display()),
        })?;
    }

    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| ClientError::Store {
            detail: format!("cannot open {}: {error}", path.display()),
        })?;

    match file.try_lock() {
        Ok(()) => Ok(Some(StreamLease { _file: file, path })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(error) => Err(ClientError::Store {
            detail: format!("cannot lock {}: {error}", path.display()),
        }),
    }
}

/// The lock file that goes with a store, beside the database so the OS can release it.
#[must_use]
pub fn lock_path(store: &Path) -> PathBuf {
    store.with_extension("stream.lock")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    fn temp_store_path(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wnk-lock-{}-{test}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("store.db")
    }

    #[test]
    fn the_first_front_end_gets_the_stream() {
        let store = temp_store_path("first");

        let lease = try_hold_stream(&store).unwrap();

        assert!(lease.is_some());
    }

    #[test]
    fn a_second_one_is_told_no_rather_than_made_to_wait() {
        // The loser must not block: `notify ls` would hang while the TUI is open.
        let store = temp_store_path("second");
        let held = try_hold_stream(&store).unwrap();
        assert!(held.is_some());

        let denied = try_hold_stream(&store).unwrap();

        assert!(denied.is_none());
    }

    #[test]
    fn the_lease_is_released_when_its_holder_goes_away() {
        let store = temp_store_path("released");
        {
            let _held = try_hold_stream(&store).unwrap().expect("the first wins");
        }

        let next = try_hold_stream(&store).unwrap();

        assert!(next.is_some(), "a dropped lease frees the stream");
    }

    /// The child half of [`critical_t28_a_killed_holder_frees_the_stream`]; a no-op unless the
    /// parent sets the variable.
    #[test]
    fn hold_the_stream_until_killed() {
        let Ok(store) = std::env::var("WNK_TEST_HOLD_STREAM") else {
            return;
        };

        let _held = try_hold_stream(Path::new(&store))
            .unwrap()
            .expect("the child is started before the parent tries");

        // libtest prints `test <name> ... ` first, so the parent searches for this token mid-line.
        println!("WNK-STREAM-HELD");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    #[test]
    fn critical_t28_a_killed_holder_frees_the_stream() {
        // T28 (§3.4): `kill -9` runs no `Drop`, so the OS must release the lock.
        let store = temp_store_path("killed");

        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "election::tests::hold_the_stream_until_killed"])
            .args(["--nocapture", "--test-threads", "1"])
            .env("WNK_TEST_HOLD_STREAM", &store)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("the test binary can spawn itself");

        let stdout = child.stdout.take().unwrap();
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let read = std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
            assert!(read > 0, "the child exited without taking the lock");
            if line.contains("WNK-STREAM-HELD") {
                break;
            }
        }

        assert!(
            try_hold_stream(&store).unwrap().is_none(),
            "the child holds it, so this front end has to read the store instead"
        );

        child.kill().expect("SIGKILL");
        child.wait().expect("reaped");

        assert!(
            try_hold_stream(&store).unwrap().is_some(),
            "a killed holder must free the stream, or a crashed tray app takes the \
             inbox down with it until somebody deletes a lock file by hand"
        );
    }

    #[test]
    fn two_profiles_do_not_elect_each_other() {
        let work = temp_store_path("work");
        let personal = temp_store_path("personal");

        let first = try_hold_stream(&work).unwrap();
        let second = try_hold_stream(&personal).unwrap();

        assert!(
            first.is_some() && second.is_some(),
            "two profiles are two servers, two devices, two streams (§9.3bis)"
        );
    }

    #[test]
    fn the_lock_sits_next_to_the_store_rather_than_inside_it() {
        let store = PathBuf::from("/data/weebo-notify/work/store.db");

        assert_eq!(
            lock_path(&store),
            PathBuf::from("/data/weebo-notify/work/store.stream.lock")
        );
    }
}
