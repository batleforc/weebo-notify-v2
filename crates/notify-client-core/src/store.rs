//! One local store per profile (§9.3bis), shared by every front end on that profile so they
//! see one inbox and one cursor. SQLite in WAL mode because several processes read while one
//! writes; which one writes is decided by [`crate::election`].

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::Connection;

use notify_client_api::types::{Action, Event, State, Status};

use crate::error::ClientError;

/// The schema this build expects; a newer store is refused, never migrated backwards.
/// 1: cache, cursor, device. 2: `created_at`/`delivered_at` (§7.2). 3: `deferred_by_dnd` (§7.4).
const SCHEMA_VERSION: i64 = 3;

/// One row of the cache, read back out of the stored event (§7.2, §9.2).
#[derive(Debug, Clone)]
pub struct CachedNotification {
    /// The notification id, which is also the cache key.
    pub id: String,
    /// Which channel it belongs to.
    pub channel: String,
    /// The headline, as of the last revision seen.
    pub title: String,
    /// The body, when the revision carried one.
    pub body: Option<String>,
    /// The revision sequence this row reflects.
    pub seq: u32,
    /// Lifecycle position as of that revision.
    pub state: State,
    /// Status as of that revision.
    pub status: Status,
    /// When the server first created it.
    pub created_at: Option<DateTime<Utc>>,
    /// When this profile first stored it; never rewritten by a later revision.
    pub delivered_at: Option<DateTime<Utc>>,
    /// Whether the server said this one was held back on purpose (design §7.4).
    pub deferred_by_dnd: bool,
    /// Buttons as of the last revision seen (§2.3), read from the stored event.
    pub actions: Vec<Action>,
}

impl CachedNotification {
    /// How long after creation this profile first saw it (§7.2); `None` when either end is
    /// missing or the delay was asked for (§7.4).
    #[must_use]
    pub fn late_by(&self) -> Option<TimeDelta> {
        if self.deferred_by_dnd {
            return None;
        }

        let (created, delivered) = (self.created_at?, self.delivered_at?);
        let late = delivered - created;

        // Negative means the two clocks disagree, not that it arrived early.
        (late > TimeDelta::zero()).then_some(late)
    }
}

/// A notification on its way into the cache.
#[derive(Debug, Clone, Copy)]
pub struct CacheEntry<'a> {
    /// The notification id.
    pub id: &'a str,
    /// Which channel it belongs to.
    pub channel: &'a str,
    /// When this revision landed, server-side. Orders the list.
    pub revised_at: &'a str,
    /// When the notification was created, server-side.
    pub created_at: &'a str,
    /// The event, verbatim as §8.1 put it on the wire.
    pub payload: &'a str,
    /// Whether the server reported this one as held back on purpose (§7.4).
    pub deferred_by_dnd: bool,
}

/// The local cache and cursor for one profile.
#[derive(Debug)]
pub struct Store {
    connection: Connection,
    path: PathBuf,
}

impl Store {
    /// Open (or create) the store for a profile.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be created, the file cannot be opened, or it
    /// was written by a schema this build does not know.
    pub fn open(profile: &str) -> Result<Self, ClientError> {
        Self::open_at(&try_store_path(profile)?)
    }

    /// The same, at an explicit path — what the tests use.
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub fn open_at(path: &Path) -> Result<Self, ClientError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| ClientError::Store {
                detail: format!("cannot create {}: {error}", parent.display()),
            })?;
            // The cache holds notification bodies; like the config and credential, owner only.
            // On the directory, so the `-wal` and `-shm` files SQLite adds are covered too.
            #[cfg(unix)]
            std::fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .map_err(|error| ClientError::Store {
                detail: format!("cannot restrict {}: {error}", parent.display()),
            })?;
        }

        let connection = Connection::open(path).map_err(|error| store_failure(&error))?;

        // Wait out a concurrent writer instead of failing with "database is locked" —
        // first, so switching to WAL below waits too when another process is mid-open.
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| store_failure(&error))?;
        // WAL: readers and the writer don't block each other (TUI streams while `notify ls` reads).
        use_wal(&connection)?;
        // NORMAL, not FULL: a lost tail is re-fetched from the cursor.
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| store_failure(&error))?;

        let store = Self {
            connection,
            path: path.to_path_buf(),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Create the schema, or refuse a file from the future.
    fn migrate(&self) -> Result<(), ClientError> {
        // One IMMEDIATE transaction around the whole upgrade, version read and write
        // included: two processes opening an old store at once take turns (the busy
        // timeout) instead of both adding the same column, and a crash half way leaves
        // the old schema rather than a half-new one that no open can get past.
        self.connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|error| store_failure(&error))?;
        let upgraded = self.upgrade();
        let end = if upgraded.is_ok() {
            "COMMIT"
        } else {
            "ROLLBACK"
        };
        self.connection
            .execute_batch(end)
            .map_err(|error| store_failure(&error))?;
        upgraded
    }

    /// The schema steps of [`Self::migrate`], inside its transaction.
    fn upgrade(&self) -> Result<(), ClientError> {
        let found: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| store_failure(&error))?;

        if found > SCHEMA_VERSION {
            return Err(ClientError::Store {
                detail: format!(
                    "{} was written by a newer client (schema {found}, this build knows \
                     {SCHEMA_VERSION}) — upgrade, or use a different --profile",
                    self.path.display()
                ),
            });
        }

        self.connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS meta (
                     key   TEXT PRIMARY KEY,
                     value TEXT NOT NULL
                 );
                 -- The cache. `payload` is the event as the API returned it,
                 -- stored whole rather than shredded into columns: the wire shape is
                 -- already the contract (ADR-0013), and a second representation here
                 -- would be one more thing to keep in step with the server.
                 CREATE TABLE IF NOT EXISTS notifications (
                     id         TEXT PRIMARY KEY,
                     channel    TEXT NOT NULL,
                     received_at TEXT NOT NULL,
                     payload    TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS notifications_by_time
                     ON notifications (received_at DESC);",
            )
            .map_err(|error| store_failure(&error))?;

        if found < 2 {
            // Schema 2 (§7.2): existing rows get "" rather than a made-up arrival time.
            self.connection
                .execute_batch(
                    "ALTER TABLE notifications
                         ADD COLUMN created_at TEXT NOT NULL DEFAULT '';
                     ALTER TABLE notifications
                         ADD COLUMN delivered_at TEXT NOT NULL DEFAULT '';",
                )
                .map_err(|error| store_failure(&error))?;
        }

        if found < 3 {
            // Schema 3 (§7.4): false for every row cached before do-not-disturb existed.
            self.connection
                .execute_batch(
                    "ALTER TABLE notifications
                         ADD COLUMN deferred_by_dnd INTEGER NOT NULL DEFAULT 0;",
                )
                .map_err(|error| store_failure(&error))?;
        }

        self.connection
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|error| store_failure(&error))
    }

    /// Where this store lives, for the lock beside it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The sync cursor, if this profile has one yet.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn cursor(&self) -> Result<Option<String>, ClientError> {
        self.meta("cursor")
    }

    /// Record the cursor, only **after** the events it covers are stored (or a crash loses them).
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be written.
    pub fn set_cursor(&self, cursor: &str) -> Result<(), ClientError> {
        self.set_meta("cursor", cursor)
    }

    /// The server this cache was filled from, once anything synced into it.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn server(&self) -> Result<Option<String>, ClientError> {
        self.meta("server")
    }

    /// Record which server fills this cache.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be written.
    pub fn set_server(&self, server: &str) -> Result<(), ClientError> {
        self.set_meta("server", server)
    }

    /// The device this profile enrolled, if any (one per profile, not per front end).
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn device(&self) -> Result<Option<String>, ClientError> {
        self.meta("device")
    }

    /// Record the enrolled device.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be written.
    pub fn set_device(&self, device: &str) -> Result<(), ClientError> {
        self.set_meta("device", device)
    }

    /// Store or replace a notification. `delivered_at` is stamped by the store on first insert
    /// and never rewritten, so the "arrived late" badge (§7.2) measures the wait, not the last edit.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be written.
    pub fn upsert(&self, entry: CacheEntry<'_>) -> Result<(), ClientError> {
        self.connection
            .execute(
                "INSERT INTO notifications
                     (id, channel, received_at, created_at, delivered_at, payload,
                      deferred_by_dnd)
                 VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?5, ?6)
                 ON CONFLICT (id) DO UPDATE
                   SET channel = excluded.channel,
                       received_at = excluded.received_at,
                       created_at = excluded.created_at,
                       payload = excluded.payload,
                       deferred_by_dnd = excluded.deferred_by_dnd",
                (
                    entry.id,
                    entry.channel,
                    entry.revised_at,
                    entry.created_at,
                    entry.payload,
                    entry.deferred_by_dnd,
                ),
            )
            .map(|_| ())
            .map_err(|error| store_failure(&error))
    }

    /// Drop one, because the server said it is gone (a tombstone, §8.1).
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be written.
    pub fn remove(&self, id: &str) -> Result<(), ClientError> {
        self.connection
            .execute("DELETE FROM notifications WHERE id = ?1", [id])
            .map(|_| ())
            .map_err(|error| store_failure(&error))
    }

    /// The cached inbox, most recent first.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn inbox(&self, limit: usize) -> Result<Vec<String>, ClientError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT payload FROM notifications
                 ORDER BY received_at DESC, id DESC
                 LIMIT ?1",
            )
            .map_err(|error| store_failure(&error))?;

        let rows = statement
            // Saturating: a negative limit would mean "everything".
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| store_failure(&error))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| store_failure(&error))
    }

    /// The cached inbox, most recent first, as an election loser draws it (§9.3bis).
    /// Rows whose event no longer parses are skipped, not fatal.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn cached_inbox(&self, limit: usize) -> Result<Vec<CachedNotification>, ClientError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, channel, created_at, delivered_at, payload, deferred_by_dnd
                 FROM notifications
                 ORDER BY received_at DESC, id DESC
                 LIMIT ?1",
            )
            .map_err(|error| store_failure(&error))?;

        let rows = statement
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            })
            .map_err(|error| store_failure(&error))?;

        let mut cached = Vec::new();
        for row in rows {
            let (id, channel, created_at, delivered_at, payload, deferred_by_dnd) =
                row.map_err(|error| store_failure(&error))?;

            // Only revisions are stored by id; anything else was written by an unreadable build.
            let Ok(Event::Revision {
                payload: content,
                seq,
                state,
                status,
                ..
            }) = serde_json::from_str::<Event>(&payload)
            else {
                continue;
            };

            cached.push(CachedNotification {
                id,
                channel,
                title: content.title,
                body: content.body,
                seq: seq.try_into().unwrap_or(1),
                state,
                status,
                created_at: timestamp(&created_at),
                delivered_at: timestamp(&delivered_at),
                deferred_by_dnd,
                actions: content.actions.map(|actions| actions.0).unwrap_or_default(),
            });
        }

        Ok(cached)
    }

    /// When this profile first received a notification (§7.2), for front ends drawing the server's list.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn delivered_at(&self, id: &str) -> Result<Option<DateTime<Utc>>, ClientError> {
        self.connection
            .query_row(
                "SELECT delivered_at FROM notifications WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .map(|raw| timestamp(&raw))
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(store_failure(&other)),
            })
    }

    /// How many notifications are cached.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn len(&self) -> Result<usize, ClientError> {
        let count: i64 = self
            .connection
            .query_row("SELECT count(*) FROM notifications", [], |row| row.get(0))
            .map_err(|error| store_failure(&error))?;
        Ok(count.try_into().unwrap_or(0))
    }

    /// Whether nothing is cached yet.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn is_empty(&self) -> Result<bool, ClientError> {
        Ok(self.len()? == 0)
    }

    fn meta(&self, key: &str) -> Result<Option<String>, ClientError> {
        self.connection
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(store_failure(&other)),
            })
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<(), ClientError> {
        self.connection
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                (key, value),
            )
            .map(|_| ())
            .map_err(|error| store_failure(&error))
    }
}

/// `<data dir>/weebo-notify/<profile>/store.db`: data, not config, so losing it costs a re-sync.
/// Falls back to the current directory without a home; anything that writes uses [`try_store_path`].
#[must_use]
pub fn store_path(profile: &str) -> PathBuf {
    try_store_path(profile).unwrap_or_else(|_| {
        PathBuf::from(".")
            .join("weebo-notify")
            .join(profile)
            .join("store.db")
    })
}

/// [`store_path`], refusing rather than falling back to the current directory, where
/// somebody else could read or have planted the file.
///
/// # Errors
///
/// When there is no home directory to resolve a data directory from.
pub fn try_store_path(profile: &str) -> Result<PathBuf, ClientError> {
    crate::config::app_dir(|dirs| dirs.data_dir())
        .map(|dir| dir.join(profile).join("store.db"))
        .ok_or_else(|| ClientError::Store {
            detail: "no home directory, so there is no data directory to keep the local \
                     store in"
                .to_owned(),
        })
}

/// Read a stored RFC 3339 timestamp; empty (schema 1) or unparseable means none, not a guess.
fn timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Switch to WAL unless the file already is (the mode persists). Switching needs the
/// file to itself and SQLite answers "busy" without waiting when another process is
/// creating it at the same moment, so a bounded retry instead of the busy timeout.
fn use_wal(connection: &Connection) -> Result<(), ClientError> {
    const ATTEMPTS: u32 = 50;
    let mut attempt = 0;
    loop {
        let mode = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .and_then(|mode| {
                if mode.eq_ignore_ascii_case("wal") {
                    Ok(mode)
                } else {
                    connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
                }
            });
        match mode {
            Ok(_) => return Ok(()),
            Err(error) if attempt < ATTEMPTS && is_busy(&error) => {
                attempt = attempt.saturating_add(1);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => return Err(store_failure(&error)),
        }
    }
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn store_failure(error: &rusqlite::Error) -> ClientError {
    ClientError::Store {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store in a directory only this test will touch.
    fn temp_store(test: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("wnk-store-{}-{test}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        let path = dir.join("store.db");
        (Store::open_at(&path).unwrap(), path)
    }

    #[test]
    fn processes_opening_a_fresh_store_at_once_all_succeed() {
        let dir = std::env::temp_dir().join(format!("wnk-store-{}-race", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        let path = dir.join("store.db");

        let opened: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || Store::open_at(&path).map(|_| ()))
            })
            .collect();

        for opening in opened {
            opening.join().unwrap().unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_cache_directory_is_the_owners_alone() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_store, path) = temp_store("mode");
        let parent = path.parent().unwrap();

        let mode = std::fs::metadata(parent).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }

    #[test]
    fn a_fresh_store_has_no_cursor_and_no_device() {
        let (store, _) = temp_store("fresh");

        assert_eq!(store.cursor().unwrap(), None);
        assert_eq!(store.device().unwrap(), None);
        assert!(store.is_empty().unwrap());
    }

    #[test]
    fn the_cursor_and_the_device_survive_reopening() {
        // A second `notify` invocation is a new process and must pick up where the last stopped.
        let (store, path) = temp_store("persist");
        store.set_cursor("evt_00000000000042").unwrap();
        store.set_device("dev_01J").unwrap();
        drop(store);

        let reopened = Store::open_at(&path).unwrap();

        assert_eq!(
            reopened.cursor().unwrap().as_deref(),
            Some("evt_00000000000042")
        );
        assert_eq!(reopened.device().unwrap().as_deref(), Some("dev_01J"));
    }

    #[test]
    fn critical_t28_deleting_the_store_asks_for_everything_again_rather_than_losing_it() {
        // T28 (§3.4): the store is only a cache. A deleted file must come back with no cursor
        // (full re-sync), never a cursor it can't justify.
        let (store, path) = temp_store("deleted");
        store.set_cursor("evt_00000000000042").unwrap();
        store.set_device("dev_01J").unwrap();
        drop(store);

        std::fs::remove_file(&path).unwrap();
        // WAL and shared-memory siblings go with it, the way `rm store.db*` would.
        for suffix in ["-wal", "-shm"] {
            let sibling = PathBuf::from(format!("{}{suffix}", path.display()));
            if sibling.exists() {
                std::fs::remove_file(sibling).unwrap();
            }
        }

        let rebuilt = Store::open_at(&path).unwrap();

        assert_eq!(
            rebuilt.cursor().unwrap(),
            None,
            "a rebuilt store must ask for the whole backlog, not resume from a place \
             it no longer knows"
        );
        assert!(rebuilt.is_empty().unwrap());
        assert_eq!(
            rebuilt.device().unwrap(),
            None,
            "the device id was in the store too: the next run enrols again rather \
             than posting receipts as a device the server may have revoked"
        );
    }

    /// One cached revision, with the fields a test cares about.
    fn entry<'a>(id: &'a str, revised_at: &'a str, payload: &'a str) -> CacheEntry<'a> {
        CacheEntry {
            id,
            channel: "ci-infra",
            revised_at,
            created_at: "2026-08-01T09:00:00Z",
            payload,
            deferred_by_dnd: false,
        }
    }

    #[test]
    fn a_notification_is_replaced_rather_than_duplicated() {
        let (store, _) = temp_store("upsert");

        store
            .upsert(entry("ntf_1", "2026-08-01T10:00:00Z", r#"{"seq":1}"#))
            .unwrap();
        store
            .upsert(entry("ntf_1", "2026-08-01T10:05:00Z", r#"{"seq":2}"#))
            .unwrap();

        assert_eq!(store.len().unwrap(), 1);
        assert_eq!(store.inbox(10).unwrap(), [r#"{"seq":2}"#]);
    }

    #[test]
    fn the_inbox_comes_back_most_recent_first() {
        let (store, _) = temp_store("order");
        store
            .upsert(entry("ntf_old", "2026-08-01T09:00:00Z", "old"))
            .unwrap();
        store
            .upsert(entry("ntf_new", "2026-08-01T11:00:00Z", "new"))
            .unwrap();

        assert_eq!(store.inbox(10).unwrap(), ["new", "old"]);
        assert_eq!(store.inbox(1).unwrap(), ["new"]);
    }

    #[test]
    fn a_tombstone_removes_it_locally() {
        let (store, _) = temp_store("tombstone");
        store
            .upsert(entry("ntf_1", "2026-08-01T10:00:00Z", "{}"))
            .unwrap();

        store.remove("ntf_1").unwrap();

        assert!(store.is_empty().unwrap());
        // Idempotent: a reconnect can replay a tombstone.
        store.remove("ntf_1").unwrap();
    }

    #[test]
    fn two_front_ends_open_the_same_file_at_once() {
        // WAL (§9.3bis): the TUI writes while `notify ls` reads.
        let (writer, path) = temp_store("shared");
        let reader = Store::open_at(&path).unwrap();

        writer
            .upsert(entry("ntf_1", "2026-08-01T10:00:00Z", "from the stream"))
            .unwrap();

        assert_eq!(reader.inbox(10).unwrap(), ["from the stream"]);
    }

    #[test]
    fn a_store_from_a_newer_client_is_refused_rather_than_downgraded() {
        let (store, path) = temp_store("future");
        store
            .connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(store);

        let refused = Store::open_at(&path);

        assert!(refused.is_err(), "a silent downgrade corrupts the cache");
    }

    /// One whole revision as §8.1 puts it on the wire (the cache is read through [`Event`]).
    fn revision(id: &str, created_at: &str, seq: u32, title: &str) -> String {
        serde_json::json!({
            "cursor": "evt_1",
            "audience": "channel",
            "channel_id": "ci-infra",
            "logged_at": created_at,
            "event": "revision",
            "notification_id": id,
            "created_at": created_at,
            "revised_at": created_at,
            "seq": seq,
            "state": "open",
            "status": "warning",
            "priority": "default",
            "author": { "kind": "user", "id": "usr_1" },
            "payload": { "title": title },
        })
        .to_string()
    }

    #[test]
    fn a_cached_row_comes_back_in_the_shape_a_front_end_draws() {
        let (store, _) = temp_store("cached");
        store
            .upsert(CacheEntry {
                id: "ntf_1",
                channel: "ci-infra",
                revised_at: "2026-08-01T10:00:00Z",
                created_at: "2026-08-01T08:00:00Z",
                payload: &revision("ntf_1", "2026-08-01T08:00:00Z", 3, "etcd degraded"),
                deferred_by_dnd: false,
            })
            .unwrap();

        let rows = store.cached_inbox(10).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "etcd degraded");
        assert_eq!(rows[0].seq, 3);
        assert_eq!(rows[0].status, Status::Warning);
        assert!(rows[0].created_at.is_some());
    }

    #[test]
    fn a_cached_rows_actions_come_back_from_the_stored_payload() {
        let (store, _) = temp_store("actions");
        let payload = serde_json::json!({
            "cursor": "evt_1",
            "audience": "channel",
            "channel_id": "ci-infra",
            "logged_at": "2026-08-01T08:00:00Z",
            "event": "revision",
            "notification_id": "ntf_1",
            "created_at": "2026-08-01T08:00:00Z",
            "revised_at": "2026-08-01T08:00:00Z",
            "seq": 1,
            "state": "open",
            "status": "warning",
            "priority": "default",
            "author": { "kind": "user", "id": "usr_1" },
            "payload": {
                "title": "etcd degraded",
                "actions": [
                    { "type": "ack", "label": "Je m'en occupe", "key": "ack-etcd" },
                ],
            },
        })
        .to_string();

        store
            .upsert(CacheEntry {
                id: "ntf_1",
                channel: "ci-infra",
                revised_at: "2026-08-01T10:00:00Z",
                created_at: "2026-08-01T08:00:00Z",
                payload: &payload,
                deferred_by_dnd: false,
            })
            .unwrap();

        let rows = store.cached_inbox(10).unwrap();

        assert_eq!(rows[0].actions.len(), 1);
        assert!(
            matches!(&rows[0].actions[0], Action::Ack { key, .. } if key.as_str() == "ack-etcd")
        );
    }

    #[test]
    fn a_cached_row_with_no_actions_comes_back_empty_rather_than_missing() {
        let (store, _) = temp_store("no_actions");
        store
            .upsert(CacheEntry {
                id: "ntf_1",
                channel: "ci-infra",
                revised_at: "2026-08-01T10:00:00Z",
                created_at: "2026-08-01T08:00:00Z",
                payload: &revision("ntf_1", "2026-08-01T08:00:00Z", 1, "no buttons here"),
                deferred_by_dnd: false,
            })
            .unwrap();

        assert!(store.cached_inbox(10).unwrap()[0].actions.is_empty());
    }

    #[test]
    fn an_unreadable_cached_row_is_skipped_rather_than_blanking_the_inbox() {
        let (store, _) = temp_store("unreadable");
        store
            .upsert(entry("ntf_bad", "2026-08-01T10:00:00Z", "not json at all"))
            .unwrap();
        store
            .upsert(CacheEntry {
                id: "ntf_good",
                channel: "ci-infra",
                revised_at: "2026-08-01T11:00:00Z",
                created_at: "2026-08-01T08:00:00Z",
                payload: &revision("ntf_good", "2026-08-01T08:00:00Z", 1, "still here"),
                deferred_by_dnd: false,
            })
            .unwrap();

        let rows = store.cached_inbox(10).unwrap();

        assert_eq!(rows.len(), 1, "one bad row must not cost the whole list");
        assert_eq!(rows[0].title, "still here");
    }

    #[test]
    fn the_arrival_time_is_recorded_once_and_survives_a_revision() {
        // A later revision must not reset the clock (§7.2).
        let (store, _) = temp_store("delivered");
        store
            .upsert(entry(
                "ntf_1",
                "2026-08-01T10:00:00Z",
                &revision("ntf_1", "2026-08-01T08:00:00Z", 1, "first"),
            ))
            .unwrap();

        let first = store.delivered_at("ntf_1").unwrap();
        assert!(first.is_some(), "the store stamps its own arrival time");

        store
            .upsert(entry(
                "ntf_1",
                "2026-08-01T12:00:00Z",
                &revision("ntf_1", "2026-08-01T08:00:00Z", 2, "second"),
            ))
            .unwrap();

        assert_eq!(store.delivered_at("ntf_1").unwrap(), first);
        assert_eq!(store.delivered_at("ntf_absent").unwrap(), None);
    }

    #[test]
    fn lateness_is_the_gap_between_creation_and_arrival() {
        let (store, _) = temp_store("late");
        store
            .upsert(entry(
                "ntf_1",
                "2026-08-01T10:00:00Z",
                // Created in 2020: whenever this test runs, the row arrived late.
                &revision("ntf_1", "2020-01-01T00:00:00Z", 1, "an old incident"),
            ))
            .unwrap();

        let rows = store.cached_inbox(10).unwrap();
        let late = rows[0].late_by().expect("both timestamps are there");

        assert!(late > TimeDelta::days(1), "{late}");
    }

    #[test]
    fn a_row_without_both_timestamps_carries_no_badge() {
        // A schema-1 row: no invented arrival time.
        let cached = CachedNotification {
            id: "ntf_1".to_owned(),
            channel: "ci-infra".to_owned(),
            title: "etcd degraded".to_owned(),
            body: None,
            seq: 1,
            state: State::Open,
            status: Status::Info,
            created_at: None,
            delivered_at: None,
            deferred_by_dnd: false,
            actions: Vec::new(),
        };

        assert_eq!(cached.late_by(), None);
    }

    #[test]
    fn a_schema_one_store_gains_the_new_columns_and_keeps_its_rows() {
        // Last week's store keeps its cache, minus arrival times.
        let (store, path) = temp_store("migrate");
        store
            .upsert(entry("ntf_1", "2026-08-01T10:00:00Z", "{}"))
            .unwrap();
        store
            .connection
            .execute_batch(
                "ALTER TABLE notifications DROP COLUMN created_at;
                 ALTER TABLE notifications DROP COLUMN delivered_at;
                 ALTER TABLE notifications DROP COLUMN deferred_by_dnd;
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(store);

        let reopened = Store::open_at(&path).unwrap();

        assert_eq!(reopened.len().unwrap(), 1, "the cache survives the upgrade");
        assert_eq!(reopened.delivered_at("ntf_1").unwrap(), None);
        assert!(
            reopened
                .cached_inbox(10)
                .unwrap()
                .iter()
                .all(|row| !row.deferred_by_dnd),
            "a row from before the flag existed was not deferred by anything"
        );
    }

    /// T10's other half: lateness that was asked for gets no badge (§7.2, §7.4).
    #[test]
    fn critical_t10_lateness_that_was_asked_for_carries_no_badge() {
        let (store, _) = temp_store("deferred");
        for (id, deferred) in [("ntf_late", false), ("ntf_quiet", true)] {
            store
                .upsert(CacheEntry {
                    id,
                    channel: "ci-infra",
                    revised_at: "2026-08-01T10:00:00Z",
                    // Eight hours before it was cached: unmistakably late.
                    created_at: "2026-08-01T02:00:00Z",
                    payload: &revision(id, "2026-08-01T02:00:00Z", 1, "etcd degraded"),
                    deferred_by_dnd: deferred,
                })
                .unwrap();
        }

        let rows = store.cached_inbox(10).unwrap();
        let badge = |id: &str| {
            rows.iter()
                .find(|row| row.id == id)
                .and_then(CachedNotification::late_by)
        };

        assert!(badge("ntf_late").is_some(), "a real delay is worth saying");
        assert!(
            badge("ntf_quiet").is_none(),
            "somebody who set quiet hours does not want to be told every morning \
             that their night was late"
        );
    }

    #[test]
    fn the_path_is_per_profile_and_under_the_data_directory() {
        let work = store_path("work");
        let personal = store_path("personal");

        assert_ne!(work, personal, "two profiles are two servers (§9.1)");
        assert!(work.ends_with("weebo-notify/work/store.db"), "{work:?}");
    }
}
