//! Waking the front ends that do not hold the stream (design §9.3bis).
//!
//! # The gap this closes
//!
//! [`crate::election`] decides which front end holds the stream and feeds the shared
//! store; the others read it. What the lock does not do is *tell* them when there is
//! something new, so a reader used to see nothing until somebody pressed a key. §9.3bis
//! asks for a local signal, and this is it: a socket beside the store that the holder
//! writes one byte to after each write.
//!
//! # One byte, and deliberately no more
//!
//! The byte carries no information and is not meant to. Putting the event in it would
//! make this a second delivery channel with its own ordering, its own backlog and its
//! own way of being wrong; the store is the channel, and this only says *look again*.
//! A reader that misses ten bytes reloads once and is correct — which is why nothing
//! here retries, buffers or acknowledges.
//!
//! # Nothing here is load-bearing
//!
//! Every failure is silent on purpose: the socket file is gone, the peer went away,
//! the platform has no unix sockets. All of them degrade to what M2 already had — the
//! tick reloads a moment later. A signal that could break the client would be a worse
//! trade than the latency it removes, so it is written to be droppable.
//!
//! On Windows the equivalent is a named pipe; it is not wired here, and the tick is
//! the whole behaviour there. That is written down rather than discovered.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::io::{ErrorKind, Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

use crate::error::ClientError;

/// The socket that goes with a store.
///
/// Beside the database and named after it, like the lock: one profile, one store,
/// one holder, one socket.
#[must_use]
pub fn signal_path(store: &Path) -> PathBuf {
    store.with_extension("stream.sock")
}

/// The holder's end: accepts readers and pokes them.
///
/// Only the front end holding the [`crate::StreamLease`] should bind one. That is
/// not enforced here because the lock already enforces it — and a second enforcement
/// that could disagree with the first is worse than none.
#[derive(Debug)]
pub struct StreamSignal {
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    peers: Vec<UnixStream>,
    path: PathBuf,
}

impl StreamSignal {
    /// Start listening beside `store`.
    ///
    /// A stale socket file — left by a holder that crashed — is removed first. That
    /// is safe precisely because the caller holds the stream lease: the operating
    /// system has already decided there is only one of them.
    ///
    /// # Errors
    ///
    /// Fails only if the socket cannot be created at all, which means the data
    /// directory is not writable and the store would not have opened either.
    pub fn bind(store: &Path) -> Result<Self, ClientError> {
        let path = signal_path(store);

        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&path);

            let listener = UnixListener::bind(&path).map_err(|error| ClientError::Store {
                detail: format!("cannot listen on {}: {error}", path.display()),
            })?;
            // Non-blocking, so accepting is something the writer does in passing
            // rather than a thread that has to be shut down cleanly.
            listener
                .set_nonblocking(true)
                .map_err(|error| ClientError::Store {
                    detail: format!("cannot set {} non-blocking: {error}", path.display()),
                })?;

            Ok(Self {
                listener,
                peers: Vec::new(),
                path,
            })
        }

        #[cfg(not(unix))]
        Ok(Self { path })
    }

    /// Tell every connected reader that the store moved.
    ///
    /// Best effort by design: a peer that has gone away is dropped rather than
    /// retried, and a reader that is not connected yet will be accepted by the next
    /// call. Nothing here can fail in a way a caller should handle — the reader's
    /// tick is the net.
    pub fn notify(&mut self) {
        #[cfg(unix)]
        {
            // Whoever connected since the last write, first: a reader that arrived a
            // moment ago should see this poke and not the one after it.
            while let Ok((peer, _)) = self.listener.accept() {
                let _ = peer.set_nonblocking(true);
                self.peers.push(peer);
            }

            self.peers.retain_mut(|peer| match peer.write_all(&[1]) {
                Ok(()) => true,
                // A full buffer means the reader has not caught up with the pokes it
                // already has, which is exactly the case where one more changes
                // nothing. Keep it; drop only what is actually broken.
                Err(error) => error.kind() == ErrorKind::WouldBlock,
            });
        }
    }

    /// Where it is listening.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StreamSignal {
    fn drop(&mut self) {
        // The socket file outlives the process otherwise, and the next holder would
        // find an address already in use. `bind` also clears it, so this is the tidy
        // path rather than the correctness one.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A reader's end: connects to the holder and reports pokes.
///
/// Reconnection is built in and silent. The holder changes when a window closes —
/// which is normal, not exceptional — and a watch that gave up the first time would
/// go quiet exactly when a second front end took over.
#[derive(Debug)]
pub struct StreamWatch {
    path: PathBuf,
    #[cfg(unix)]
    stream: Option<UnixStream>,
}

impl StreamWatch {
    /// Watch the signal beside `store`. Never fails: nobody listening yet is the
    /// ordinary state, not an error.
    #[must_use]
    pub fn watch(store: &Path) -> Self {
        let mut watch = Self {
            path: signal_path(store),
            #[cfg(unix)]
            stream: None,
        };
        watch.connect();
        watch
    }

    /// Whether the holder wrote something since the last call.
    ///
    /// Never blocks. Several pokes collapse into one answer, which is the whole
    /// point: the reader reloads once whatever landed.
    pub fn poked(&mut self) -> bool {
        #[cfg(unix)]
        {
            if self.stream.is_none() {
                self.connect();
            }

            let Some(stream) = self.stream.as_mut() else {
                return false;
            };

            let mut buffer = [0_u8; 64];
            match stream.read(&mut buffer) {
                // Nothing to read. The ordinary answer.
                Err(error) if error.kind() == ErrorKind::WouldBlock => false,
                // The holder went away. Drop it, and the next call reconnects to
                // whoever took over.
                Ok(0) | Err(_) => {
                    self.stream = None;
                    false
                }
                Ok(_) => {
                    // Drained to the end, so a burst of pokes is one reload rather
                    // than one per byte.
                    while matches!(stream.read(&mut buffer), Ok(count) if count > 0) {}
                    true
                }
            }
        }

        #[cfg(not(unix))]
        false
    }

    /// Whether a holder is currently listening. Shown, not acted on.
    #[must_use]
    pub fn connected(&self) -> bool {
        #[cfg(unix)]
        {
            self.stream.is_some()
        }

        #[cfg(not(unix))]
        false
    }

    fn connect(&mut self) {
        #[cfg(unix)]
        {
            self.stream = UnixStream::connect(&self.path).ok().inspect(|stream| {
                let _ = stream.set_nonblocking(true);
            });
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    fn temp_store_path(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wnk-signal-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("store.db")
    }

    #[test]
    fn a_reader_learns_that_the_holder_wrote() {
        let store = temp_store_path("poke");
        let mut signal = StreamSignal::bind(&store).unwrap();
        let mut watch = StreamWatch::watch(&store);

        assert!(!watch.poked(), "nothing has happened yet");

        signal.notify();

        assert!(watch.poked(), "a reader that misses this polls instead");
        assert!(!watch.poked(), "and a poke is consumed, not repeated");
    }

    #[test]
    fn a_burst_of_writes_is_one_reload() {
        // Forty revisions of one deploy must not mean forty reloads.
        let store = temp_store_path("burst");
        let mut signal = StreamSignal::bind(&store).unwrap();
        let mut watch = StreamWatch::watch(&store);

        for _ in 0..40 {
            signal.notify();
        }

        assert!(watch.poked());
        assert!(!watch.poked());
    }

    #[test]
    fn a_reader_that_arrives_late_is_still_accepted() {
        // The holder binds when it wins the election, which is before anybody else
        // has started: connecting later is the normal order of events.
        let store = temp_store_path("late");
        let mut signal = StreamSignal::bind(&store).unwrap();

        let mut watch = StreamWatch::watch(&store);
        signal.notify();

        assert!(watch.poked());
    }

    #[test]
    fn nobody_listening_is_an_answer_and_not_a_failure() {
        // The state a front end is in before any holder exists, and after one exits.
        let store = temp_store_path("absent");

        let mut watch = StreamWatch::watch(&store);

        assert!(!watch.connected());
        assert!(!watch.poked(), "and it must not spin or panic");
    }

    #[test]
    fn a_holder_that_goes_away_takes_its_socket_with_it() {
        let store = temp_store_path("gone");
        {
            let signal = StreamSignal::bind(&store).unwrap();
            assert!(signal.path().exists());
        }

        assert!(
            !signal_path(&store).exists(),
            "a leftover socket is an address the next holder cannot bind"
        );
    }

    #[test]
    fn a_stale_socket_does_not_stop_the_next_holder() {
        // What a `kill -9` leaves behind. The lease has already decided there is only
        // one holder, so clearing it is safe.
        let store = temp_store_path("stale");
        std::fs::write(signal_path(&store), b"stale").unwrap();

        assert!(StreamSignal::bind(&store).is_ok());
    }

    #[test]
    fn the_socket_sits_next_to_the_store_like_the_lock() {
        assert_eq!(
            signal_path(Path::new("/data/weebo-notify/work/store.db")),
            PathBuf::from("/data/weebo-notify/work/store.stream.sock")
        );
    }
}
