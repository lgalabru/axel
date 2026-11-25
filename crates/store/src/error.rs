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

    /// The database is newer than this build understands.  It was left
    /// untouched; `backup` is the copy taken before it was upgraded past
    /// `supported`, when one exists.
    #[error("{}", too_new_message(*.found, *.supported, .backup.as_deref()))]
    SchemaTooNew {
        found: i64,
        supported: i64,
        backup: Option<PathBuf>,
    },

    /// Migration `version` in the database is not the one this build would
    /// apply (another branch's build migrated it).  It was left untouched.
    #[error(
        "database migration {version} has fingerprint {found}, but this build's migration {version} is {expected}: \
         the database was upgraded by a different build of axel. Run that build, or one that includes its \
         migrations. The database was not changed; do not delete it."
    )]
    SchemaDiverged {
        version: i64,
        found: String,
        expected: String,
    },

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

/// The `SchemaTooNew` message: what happened, what to do, and never "delete
/// the database", which is the fix that loses every thread.
fn too_new_message(found: i64, supported: i64, backup: Option<&std::path::Path>) -> String {
    let mut message = format!(
        "database schema version {found} is newer than this build of axel supports ({supported}). \
         Install the newer axel that upgraded it. The database was not changed; do not delete it."
    );
    if let Some(backup) = backup {
        message.push_str(&format!(
            " A copy taken at schema version {supported}, before that upgrade, is at {}.",
            backup.display()
        ));
    }
    message
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
