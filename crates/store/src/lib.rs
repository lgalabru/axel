//! axel-store: the event log and its projections, shared by CLI, hooks and TUI.
//!
//! A thread is an append-only log of signed envelopes ([`Envelope`]); every
//! table that describes threads, sessions, status and usage is a projection of
//! that log and can be rebuilt with [`Store::rebuild_projections`].  Two
//! replicas merge by exchanging envelopes ([`Store::events_since`] →
//! [`Store::append`]) and converge to identical projections whatever the
//! arrival order.  See docs/DESIGN.md "Multiplayer" and docs/CONTRACTS.md.
//!
//! Local-only state (this machine's environment, projects, agent registry and
//! read frontiers) lives next to the log and is never an envelope.
//!
//! ```no_run
//! use axel_store::{Store, Keypair};
//! let store = Store::open_default()?;
//! let owner = Keypair::generate();
//! let thread = store.create_thread(&owner, "Ship it", "Make the release", None)?;
//! assert_eq!(store.participants(&thread.id)?.len(), 1);
//! # Ok::<(), axel_store::Error>(())
//! ```

mod error;
mod guard;
mod items;
mod local;
mod native_usage;
mod log;
mod policy;
mod reads;
mod remote;
pub mod pricing;
pub mod schema;
mod types;
pub mod wire;
mod writers;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use rusqlite::Connection;

pub use axel_identity::Keypair;
pub use axel_inbox::{Envelope, EnvelopeBuilder};
pub use error::Error;
pub use log::author;
pub use remote::{ItemChange, SyncStream};
pub use policy::{
    AgentPolicy, Budget, DEFAULT_MAX_DEPTH, Reach, Refusal, SpendLimit, SummonCheck, local_day_start, narrower,
};
pub use guard::{SCHEMA_BACKUPS_KEPT_MAX, schema_backup_path};
pub use schema::SCHEMA_VERSION;
pub use types::{
    Agent, AgentColor, Appended, BlockedKind, ConversationItem, Effort, Environment, Harness, ItemKind, parse_harness_model,
    ItemStatus, NewAgent, NewItem, NewSession, Participant, ParticipantRole, Project, Relationship,
    RuntimeMode, Session, SessionFilter, SessionRuntime, SessionState, SessionUpdate, Status,
    StatusReport, StatusState, Thread, ThreadFilter, ThreadProject, ThreadState, ThreadSummary,
    ThreadWorkspace, Turn, TurnChanges, TurnMark, FileChange, ChangeStatus, TurnSummary, Usage, UsageDelta, UsageSample, UsageTotals, UsageWindows, NativeUsage, SessionContext, LimitWindow, PlanLimit, WorkspaceKind,
};
pub use wire::kind;

/// The read-frontier scope used by [`Store::unread_count`] and [`Store::mark_read`].
pub const INBOX_SCOPE: &str = "inbox";

/// How long `usage_samples` rows are kept, in seconds.
pub const USAGE_SAMPLE_RETENTION_SECS: i64 = SECONDS_PER_DAY;

pub(crate) const SECONDS_PER_HOUR: i64 = 60 * 60;
pub(crate) const SECONDS_PER_DAY: i64 = 24 * SECONDS_PER_HOUR;

/// How long a connection waits on another process's write lock before
/// failing with `SQLITE_BUSY`.  Writes are single small transactions, so
/// five seconds only runs out when something holds the lock abnormally.
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// Current Unix time in seconds.
pub(crate) fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A row count or limit as SQLite's `i64`.  Saturates: no table comes near
/// `i64::MAX` rows, so the clamp never changes a result.
pub(crate) fn sql_limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

/// An unsigned value for an SQLite integer column, refused (as `what`) when
/// it does not fit in an `i64` rather than wrapped to a negative number.
pub(crate) fn sql_integer(value: u64, what: &str) -> Result<i64, Error> {
    i64::try_from(value).map_err(|_| Error::Invalid(format!("{what} {value} is out of range")))
}

/// An integer column read as an unsigned (or narrower) type; a stored value
/// out of its range is an error, not a wrapped number.
pub(crate) fn get_integer<T: TryFrom<i64>>(
    row: &rusqlite::Row<'_>,
    column: &str,
) -> rusqlite::Result<T> {
    let value: i64 = row.get(column)?;
    match T::try_from(value) {
        Ok(converted) => Ok(converted),
        Err(_) => Err(rusqlite::Error::IntegralValueOutOfRange(
            row.as_ref().column_index(column)?,
            value,
        )),
    }
}

/// A fresh ULID string (26 chars, Crockford base32, time-ordered).
pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// Handle to one open database.  Not `Sync`: open one per thread.
pub struct Store {
    conn: Connection,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// `$AXEL_DB`, else `$AXEL_HOME/axel.db`, else `~/.axel/axel.db`
    /// (`./.axel/axel.db` if there is no home directory).
    pub fn default_path() -> PathBuf {
        if let Some(p) = std::env::var_os("AXEL_DB")
            && !p.is_empty()
        {
            return PathBuf::from(p);
        }
        if let Some(home) = std::env::var_os("AXEL_HOME")
            && !home.is_empty()
        {
            return PathBuf::from(home).join("axel.db");
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".axel")
            .join("axel.db")
    }

    /// Open (creating if needed) the database at `path`, set pragmas and run migrations.
    ///
    /// Opening never loses data: a database this build is about to upgrade is
    /// first copied to [`schema_backup_path`]; one from a newer or a different
    /// build is refused untouched; and a `-wal`/`-shm` left behind by a deleted
    /// database is moved aside rather than read into the new one.
    pub fn open(path: &Path) -> Result<Store, Error> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        guard::set_aside_orphaned_sidecars(path)?;
        let conn = Connection::open(path)?;
        Store::init(conn, Some(path)).map_err(|error| match error {
            Error::SchemaTooNew { found, supported, backup: None } => {
                let candidate = schema_backup_path(path, supported);
                let backup = candidate.try_exists().unwrap_or(false).then_some(candidate);
                Error::SchemaTooNew { found, supported, backup }
            }
            other => other,
        })
    }

    /// [`Store::open`] at [`Store::default_path`].
    pub fn open_default() -> Result<Store, Error> {
        Store::open(&Store::default_path())
    }

    /// A private in-memory database with the full schema.  Used by tests and dry runs.
    pub fn open_memory() -> Result<Store, Error> {
        Store::init(Connection::open_in_memory()?, None)
    }

    /// `disk` is the file `conn` was opened from, or `None` in memory.
    fn init(mut conn: Connection, disk: Option<&Path>) -> Result<Store, Error> {
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
        if disk.is_some() {
            // Returns the resulting mode; "wal" on success, something else on
            // filesystems that cannot do WAL — not an error for us.
            let _: String =
                conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        if let Some(path) = disk {
            // A failed copy stops the upgrade: better an error today than a
            // migration that can't be undone.
            guard::backup_before_upgrade(&conn, path)?;
        }
        schema::migrate(&mut conn)?;
        Ok(Store { conn })
    }

    /// Wrap an already-migrated connection (tests stage older schemas this way).
    #[cfg(test)]
    pub(crate) fn from_connection(conn: Connection) -> Store {
        Store { conn }
    }

    /// The schema version of the open database (equals [`SCHEMA_VERSION`] after open).
    pub fn schema_version(&self) -> Result<i64, Error> {
        schema::current_version(&self.conn)
    }

    /// Raw connection, for ad-hoc queries by the `axel` binary.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}
