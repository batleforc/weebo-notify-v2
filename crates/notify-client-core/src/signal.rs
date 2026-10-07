//! Wakes the front ends that don't hold the stream (§9.3bis): the holder writes one
//! meaningless byte to a socket beside the store after each write, and readers reload.
//! Every failure is silent; the reader's tick is the fallback (and the whole story on Windows).

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::io::{ErrorKind, Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

use crate::error::ClientError;

/// The socket that goes with a store, beside the database like the lock.
#[must_use]
pub fn signal_path(store: &Path) -> PathBuf {
    store.with_extension("stream.sock")
}

/// The holder's end: accepts readers and pokes them (only the lease holder binds one).
#[derive(Debug)]
pub struct StreamSignal {
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    peers: Vec<UnixStream>,
    path: PathBuf,
}

impl StreamSignal {
    /// Start listening beside `store`, removing a stale socket first (safe: the caller holds the lease).
    ///
    /// # Errors
    ///
    /// Fails only if the socket cannot be created (unwritable data directory).
    pub fn bind(store: &Path) -> Result<Self, ClientError> {
        let path = signal_path(store);

        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&path);

            let listener = UnixListener::bind(&path).map_err(|error| ClientError::Store {
                detail: format!("cannot listen on {}: {error}", path.display()),
            })?;
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

    /// Tell every connected reader that the store moved (best effort; broken peers are dropped).
    pub fn notify(&mut self) {
        #[cfg(unix)]
        {
            // Accept new readers first so they see this poke.
            while let Ok((peer, _)) = self.listener.accept() {
                let _ = peer.set_nonblocking(true);
                self.peers.push(peer);
            }

            self.peers.retain_mut(|peer| match peer.write_all(&[1]) {
                Ok(()) => true,
                // A full buffer: the reader already has unread pokes, so keep it.
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
        // Tidy-up only; `bind` also clears a stale socket.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A reader's end: connects to the holder and reports pokes, reconnecting silently when the
/// holder changes.
#[derive(Debug)]
pub struct StreamWatch {
    path: PathBuf,
    #[cfg(unix)]
    stream: Option<UnixStream>,
}

impl StreamWatch {
    /// Watch the signal beside `store`; never fails (no holder yet is the ordinary state).
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

    /// Whether the holder wrote something since the last call; never blocks, pokes collapse into one.
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
                Err(error) if error.kind() == ErrorKind::WouldBlock => false,
                // The holder went away; the next call reconnects to whoever took over.
                Ok(0) | Err(_) => {
                    self.stream = None;
                    false
                }
                Ok(_) => {
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
        // What a `kill -9` leaves behind.
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
