//! Error type for `axel-store`.

use std::path::PathBuf;

/// Everything that can go wrong inside the store.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A SQLite error that is not one of the mapped cases below.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A JSON column or envelope content could not be (de)serialised.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// Filesystem trouble, such as creating `~/.axel`.
    #[error("io at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The database is newer than this build understands.
    #[error("database schema version {found} is newer than supported version {supported}")]
    SchemaTooNew { found: i64, supported: i64 },

    /// A uniqueness rule was violated (for example an agent name already in use).
    #[error("conflict: {0}")]
    Conflict(String),

    /// A row that must exist does not.
    #[error("not found: {0}")]
    NotFound(String),

    /// A stored value could not be interpreted (bad enum string, bad path, ...).
    #[error("corrupt row: {0}")]
    Corrupt(String),

    /// The envelope's id or signature does not check out; it was not stored.
    #[error("invalid envelope: {0}")]
    Envelope(#[from] axel_inbox::Error),

    /// A key or attestation problem.
    #[error("identity: {0}")]
    Identity(#[from] axel_identity::Error),

    /// The envelope was stored but not projected: its signer may not act on the thread.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// A caller-side argument problem.
    #[error("invalid argument: {0}")]
    Invalid(String),
}

impl Error {
    /// Map a SQLite UNIQUE/PRIMARY KEY violation to [`Error::Conflict`] with `what` as the message.
    pub(crate) fn conflict_or(err: rusqlite::Error, what: impl FnOnce() -> String) -> Error {
        match &err {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation
                    && matches!(
                        f.extended_code,
                        rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                            | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                    ) =>
            {
                Error::Conflict(what())
            }
            _ => Error::Sqlite(err),
        }
    }
}
