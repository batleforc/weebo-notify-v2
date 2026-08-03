//! Which front end holds the stream (design §9.3bis).
//!
//! # One connection, several front ends
//!
//! The CLI, the TUI and the desktop app can all be running against the same
//! profile. Exactly one of them should hold the SSE connection and feed the shared
//! store; the rest read it. Without that rule, three windows on one laptop spend
//! three of the twenty stream slots a user gets across every device they own
//! (§8.2), and each keeps a cursor the others cannot see.
//!
//! # An advisory file lock, and why not anything cleverer
//!
//! The holder is whoever holds an exclusive lock on a file next to the store. That
//! choice is the whole design:
//!
//! * **a process that dies releases it by ceasing to exist** — crash, `kill -9`,
//!   a laptop lid closing on a suspended process. A flag in a table would need a
//!   lease, a heartbeat, and a recovery path for the holder that never came back,
//!   all to answer a question the operating system answers for free;
//! * **it is per profile**, because the store is (§9.3bis): two profiles are two
//!   servers, two devices, two streams, and they must not elect each other;
//! * **acquiring is non-blocking**. A front end that loses the election does not
//!   wait — it reads the store and gets on with it, which is what makes the desktop
//!   app in the tray naturally win by simply being there first.
//!
//! What it does not do is *notify* the losers when something lands. §9.3bis asks
//! for a local signal — a unix socket or a named pipe — so the TUI redraws the
//! instant the tray app writes. That is not here: without it a reader polls, which
//! is a latency problem rather than a correctness one, and it is written down in
//! §14.1 rather than pretended away.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::error::ClientError;

/// Proof that this process holds the stream for a profile.
///
/// Dropping it releases the lock, so the next front end to try wins. There is no
/// `release()`: a lock you can forget to release is a lock that outlives its
/// process on the day somebody adds an early `return`.
#[derive(Debug)]
pub struct StreamLease {
    /// Held for its `Drop`. The lock lives on the open descriptor, so closing the
    /// file *is* the release.
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

/// Try to become the front end that holds the stream.
///
/// Returns `None` when somebody else already does — which is an ordinary answer,
/// not a failure: the caller reads the shared store instead.
///
/// # Errors
///
/// Fails only if the lock file cannot be created or opened at all, which means the
/// data directory is not writable and the store would not have opened either.
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

    // `std::fs::File::try_lock` since Rust 1.89 — no crate needed for what the
    // operating system has always done.
    match file.try_lock() {
        Ok(()) => Ok(Some(StreamLease { _file: file, path })),
        // Somebody else holds it. Not an error: this front end reads the store.
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(error) => Err(ClientError::Store {
            detail: format!("cannot lock {}: {error}", path.display()),
        }),
    }
}

/// The lock that goes with a store.
///
/// Beside the database rather than inside it: a file the operating system can
/// release on its own is the entire point (see the module docs).
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
        let _ = std::fs::remove_dir_all(&dir);
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
        // The losing front end reads the store and carries on; blocking here would
        // hang `notify ls` for as long as the TUI is open.
        let store = temp_store_path("second");
        let held = try_hold_stream(&store).unwrap();
        assert!(held.is_some());

        let denied = try_hold_stream(&store).unwrap();

        assert!(denied.is_none());
    }

    #[test]
    fn the_lease_is_released_when_its_holder_goes_away() {
        // The property that makes this an advisory lock rather than a flag: no
        // heartbeat, no lease expiry, no recovery path for a holder that crashed.
        let store = temp_store_path("released");
        {
            let _held = try_hold_stream(&store).unwrap().expect("the first wins");
        }

        let next = try_hold_stream(&store).unwrap();

        assert!(next.is_some(), "a dropped lease frees the stream");
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
