//! One local store per profile (design §9.3bis).
//!
//! # What this exists to prevent
//!
//! The CLI, the TUI and the desktop app are three front ends over one account. If
//! each kept its own cache and its own cursor, the same person would see three
//! different inboxes, mark something read in one and find it unread in the next,
//! and hold three SSE connections against a budget of twenty per user (§8.2). So
//! there is **one store per profile**, at a standard path, and every front end
//! pointing at that profile opens the same file.
//!
//! The scope is the *profile*, not the machine (§9.1 `--profile`, one per server):
//! two profiles are two servers, two devices, two cursors, two locks. Sharing a
//! store between them would mean one server's notifications appearing under
//! another's cursor.
//!
//! # Why SQLite, and what it costs
//!
//! §9.3bis asks for SQLite in WAL mode, and the reason is the requirement above:
//! several *processes* read while one writes. That is what WAL does and what the
//! pure-Rust embedded stores do not — `redb` and friends take an exclusive lock on
//! the file, which would turn "the TUI is open" into "the CLI cannot read".
//!
//! The cost is honest and worth stating: `rusqlite` with `bundled` compiles SQLite
//! from C, so building a **client** needs a C compiler. That is not the trade this
//! workspace refused for the *server* (jsonwebtoken's pure-Rust backend, so the
//! container image builds from a plain `cargo build`) — a CLI is not a container
//! image, and SQLite is the most portable C there is.
//!
//! # The lock is an election, not a mutex
//!
//! One front end holds the stream and feeds the store; the others read it (§9.3bis).
//! Which one is decided by an **advisory file lock** next to the database, so a
//! process that dies — crash, `kill -9`, laptop lid — releases it by ceasing to
//! exist. A flag in a table would need a lease, a heartbeat and a recovery path for
//! the case where the holder never came back, all to answer a question the
//! operating system already answers.

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::Connection;

use notify_client_api::types::{Event, State, Status};

use crate::error::ClientError;

/// The schema this build expects.
///
/// A store written by a newer client is left alone rather than migrated backwards:
/// an older front end refuses to open it and says so, which is a worse afternoon
/// for one person than a corrupted cache is for all of them.
///
/// * **1** — the cache, the cursor and the device.
/// * **2** — `created_at` and `delivered_at`, for the "arrived late" badge (§7.2).
/// * **3** — `deferred_by_dnd`, so the badge is not shown for lateness that was
///   asked for (§7.4).
const SCHEMA_VERSION: i64 = 3;

/// One row of the cache, in the shape a front end draws (design §7.2, §9.2).
///
/// Read back out of the stored event rather than re-fetched: a front end that does
/// not hold the stream renders from here (§9.3bis), and the whole point is that it
/// does so without a request.
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
    /// When **this** profile first stored it — never rewritten by a later revision,
    /// which is what makes the lateness below a property of the arrival rather than
    /// of the last edit.
    pub delivered_at: Option<DateTime<Utc>>,
    /// Whether the server said this one was held back on purpose (design §7.4).
    pub deferred_by_dnd: bool,
}

impl CachedNotification {
    /// How long after its creation this profile first saw it (design §7.2).
    ///
    /// `None` when either end is missing — a row cached by a schema-1 build has no
    /// arrival time, and inventing one would put a badge on every old row — and
    /// `None` when the wait was **asked for** (§7.4). That last case is the point of
    /// the flag: somebody who set quiet hours does not want to be told every morning
    /// that their night was late.
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
///
/// A struct rather than six positional strings: `upsert(id, channel, a, b, c, d)`
/// is a call nobody can read, and two timestamps of the same type next to each
/// other is exactly where they get swapped.
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
    ///
    /// Passed in rather than stamped by the store, unlike `delivered_at`: this is a
    /// fact about the *server's* view of a preference, not about this cache.
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
        Self::open_at(&store_path(profile))
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
        }

        let connection = Connection::open(path).map_err(|error| store_failure(&error))?;

        // WAL is the whole reason SQLite is here: readers do not block the writer
        // and the writer does not block readers, which is what lets the TUI hold
        // the stream while `notify ls` reads the same file.
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| store_failure(&error))?;
        // NORMAL rather than FULL: a cache that loses the last few events to a
        // power cut re-fetches them from the cursor, and paying an fsync per write
        // for that is a poor trade.
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| store_failure(&error))?;
        // Without this, a second front end writing at the same moment gets an
        // immediate "database is locked" instead of waiting the moment out.
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
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
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS meta (
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
                     ON notifications (received_at DESC);
                 COMMIT;",
            )
            .map_err(|error| store_failure(&error))?;

        if found < 2 {
            // Schema 2 (design §7.2): when the server made it, and when *this*
            // profile first saw it. Existing rows get the empty string rather than a
            // guess — a made-up arrival time would put an "arrived late" badge on
            // every row a schema-1 build had already cached.
            self.connection
                .execute_batch(
                    "BEGIN;
                     ALTER TABLE notifications
                         ADD COLUMN created_at TEXT NOT NULL DEFAULT '';
                     ALTER TABLE notifications
                         ADD COLUMN delivered_at TEXT NOT NULL DEFAULT '';
                     COMMIT;",
                )
                .map_err(|error| store_failure(&error))?;
        }

        if found < 3 {
            // Schema 3 (design §7.4): lateness that was asked for. Defaults to
            // false, which is the truth for every row cached before there was a
            // do-not-disturb to defer anything.
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

    /// Record the cursor.
    ///
    /// Written **after** the events it covers are stored, never before: the other
    /// order means a crash in between loses events the cursor now claims were seen,
    /// and nothing will ever fetch them again.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be written.
    pub fn set_cursor(&self, cursor: &str) -> Result<(), ClientError> {
        self.set_meta("cursor", cursor)
    }

    /// The device this profile enrolled, if any.
    ///
    /// One device per profile rather than per front end (§9.3bis) — which is also
    /// what makes "seen by 3/5" count this machine once instead of three times.
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

    /// Store or replace a notification.
    ///
    /// `delivered_at` is stamped by the **store**, on the first insertion and never
    /// again. Both halves matter:
    ///
    /// * the store stamps it because the arrival time is a fact about this machine's
    ///   cache, not something a caller should be able to pass in wrong;
    /// * it is never rewritten because a revision landing forty minutes later must
    ///   not reset it, or the "arrived late" badge of §7.2 would measure the last
    ///   edit instead of the wait. That is why the conflict clause names its columns
    ///   one by one rather than replacing the row.
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
            // Saturating rather than `as`: a limit that wrapped negative would turn
            // "show me a page" into "show me everything".
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| store_failure(&error))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| store_failure(&error))
    }

    /// The cached inbox, most recent first, in the shape a front end draws.
    ///
    /// This is what a front end that lost the stream election reads (§9.3bis): the
    /// holder writes, everybody else renders from here, and nobody spends a second
    /// stream slot to show the same list.
    ///
    /// A row whose stored event no longer parses is **skipped rather than fatal** —
    /// one unreadable cache entry must not blank an inbox.
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

            // Only a revision has anything to draw, and only a revision is ever
            // stored under a notification's id — anything else here is a row written
            // by a build whose events this one cannot read.
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
            });
        }

        Ok(cached)
    }

    /// When this profile first received each of these notifications (design §7.2).
    ///
    /// Separate from [`Self::cached_inbox`] because a front end that renders the
    /// *server's* list still wants the badge, and the arrival time is the one thing
    /// only the local store knows.
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

/// `$XDG_DATA_HOME/weebo-notify/<profile>/store.db`, or the usual fallback.
///
/// Data, not config: a cache belongs where a backup tool can skip it, and where a
/// `rm -rf` costs a re-sync rather than a re-configuration.
#[must_use]
pub fn store_path(profile: &str) -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));

    base.join("weebo-notify").join(profile).join("store.db")
}

/// Read a stored RFC 3339 timestamp, or nothing.
///
/// The empty string is the schema-1 row that never had one, and an unparseable value
/// is treated the same way: a missing timestamp costs a badge, a guessed one is
/// wrong information.
fn timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn store_failure(error: &rusqlite::Error) -> ClientError {
    ClientError::Store {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    /// A store in a directory only this test will touch.
    fn temp_store(test: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("wnk-store-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.db");
        (Store::open_at(&path).unwrap(), path)
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
        // The point of the file: a second `notify` invocation is a different
        // process and has to pick up where the last one stopped.
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
        // Revisions arrive as new versions of the same id (§2.2), so upserting is
        // the normal path and not an edge case.
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
        // Removing what is already gone is the desired state: a tombstone can be
        // replayed by a reconnect.
        store.remove("ntf_1").unwrap();
    }

    #[test]
    fn two_front_ends_open_the_same_file_at_once() {
        // The requirement WAL is here for (§9.3bis): the TUI holds the stream and
        // writes, `notify ls` reads, and neither waits for the other.
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

    /// One revision as §8.1 puts it on the wire, created `created_at`.
    ///
    /// Whole rather than "the fields this test reads": since D1 the cache is read
    /// back through the generated [`Event`], so a half-written fixture would be
    /// testing the parser's tolerance instead of the store's behaviour.
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
        // What a front end that lost the stream election renders from (§9.3bis) —
        // without a request, which is the entire point of the shared store.
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
        // The property the "arrived late" badge rests on (§7.2): a revision landing
        // later must not reset the clock, or the badge would measure the last edit.
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
        // A row cached by a schema-1 build. Inventing an arrival time would put a
        // badge on every one of them.
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
        };

        assert_eq!(cached.late_by(), None);
    }

    #[test]
    fn a_schema_one_store_gains_the_new_columns_and_keeps_its_rows() {
        // The migration a person with a store from last week goes through: the cache
        // is not thrown away, it just has no arrival times for what is already in it.
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
