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
mod items;
mod local;
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
pub use schema::SCHEMA_VERSION;
pub use types::{
    Agent, AgentColor, Appended, BlockedKind, ConversationItem, Environment, Harness, ItemKind, parse_harness_model,
    ItemStatus, NewAgent, NewItem, NewSession, Participant, ParticipantRole, Project, Relationship,
    RuntimeMode, Session, SessionFilter, SessionRuntime, SessionState, SessionUpdate, Status,
    StatusReport, StatusState, Thread, ThreadFilter, ThreadProject, ThreadState, ThreadSummary,
    ThreadWorkspace, Turn, TurnSummary, Usage, UsageDelta, UsageSample, UsageTotals, UsageWindows, SessionContext, LimitWindow, PlanLimit, WorkspaceKind,
};
pub use wire::kind;

/// The read-frontier scope used by [`Store::unread_count`] and [`Store::mark_read`].
pub const INBOX_SCOPE: &str = "inbox";

/// How long `usage_samples` rows are kept, in seconds.
pub const USAGE_SAMPLE_RETENTION_SECS: i64 = 24 * 60 * 60;

/// Current Unix time in seconds.
pub(crate) fn now() -> i64 {
    chrono::Utc::now().timestamp()
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
    pub fn open(path: &Path) -> Result<Store, Error> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        Store::init(conn, true)
    }

    /// [`Store::open`] at [`Store::default_path`].
    pub fn open_default() -> Result<Store, Error> {
        Store::open(&Store::default_path())
    }

    /// A private in-memory database with the full schema.  Used by tests and dry runs.
    pub fn open_memory() -> Result<Store, Error> {
        Store::init(Connection::open_in_memory()?, false)
    }

    fn init(mut conn: Connection, on_disk: bool) -> Result<Store, Error> {
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        if on_disk {
            // Returns the resulting mode; "wal" on success, something else on
            // filesystems that cannot do WAL — not an error for us.
            let _: String =
                conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
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
