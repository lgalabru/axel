//! What [`crate::Store::open`] does to a database file on disk before it
//! migrates it, so that upgrading Axel can never cost the owner their threads.
//!
//! - A database about to be upgraded is first copied to `axel.db.v<N>.bak`,
//!   where `N` is the schema version of the copy.  An older build refused by
//!   a newer database names that copy, and a failed or unwanted upgrade can
//!   be undone by moving it back.  The newest [`SCHEMA_BACKUPS_KEPT_MAX`]
//!   copies are kept.
//! - A WAL or shared-memory file left behind when the main file is gone
//!   belongs to a database that no longer exists here.  SQLite would read it
//!   as part of the new, empty one, so it is moved aside, never deleted: the
//!   WAL can hold the deleted database's latest writes.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::{Error, schema};

/// How many pre-upgrade copies to keep.  Two lets the owner step back over
/// the last two upgrades; each copy is as large as the database itself
/// (hundreds of megabytes for a busy one), so more costs real disk.
pub const SCHEMA_BACKUPS_KEPT_MAX: usize = 2;

/// The most directory entries read while looking for old copies to prune.
/// `~/.axel` holds a few dozen files; this only stops a pathological one.
const DIRECTORY_ENTRIES_MAX: usize = 10_000;

/// SQLite's sidecar files for a WAL-mode database at `path`.
const SIDECAR_SUFFIXES: [&str; 2] = ["-wal", "-shm"];

/// The copy of the database at `path` taken at schema `version`.
pub fn schema_backup_path(path: &Path, version: i64) -> PathBuf {
    assert!(version > 0, "only a migrated database is backed up, not version {version}");
    suffixed(path, &format!(".v{version}.bak"))
}

/// When the database file at `path` is missing, move any `-wal` or `-shm`
/// file it left behind to `<name>.orphan-<unix seconds>`.  Returns where
/// each one went.
pub(crate) fn set_aside_orphaned_sidecars(path: &Path) -> Result<Vec<PathBuf>, Error> {
    if exists(path)? {
        return Ok(Vec::new());
    }
    let mut moved = Vec::new();
    for suffix in SIDECAR_SUFFIXES {
        let sidecar = suffixed(path, suffix);
        if !exists(&sidecar)? {
            continue;
        }
        let aside = suffixed(&sidecar, &format!(".orphan-{}", crate::now()));
        std::fs::rename(&sidecar, &aside).map_err(|source| io(&sidecar, source))?;
        moved.push(aside);
    }
    assert!(moved.len() <= SIDECAR_SUFFIXES.len());
    Ok(moved)
}

/// Copy the database at `path` to [`schema_backup_path`] when this build is
/// about to upgrade it.  Returns the copy, or `None` when there is nothing to
/// upgrade (a new, current or too-new database).
///
/// The copy is a `VACUUM INTO`: a consistent snapshot, WAL included, taken
/// while other processes keep reading and writing.  If another process
/// upgrades the database between our version check and the snapshot, the
/// snapshot is already upgraded; it is then discarded, because that process
/// took the real copy before it migrated.
pub(crate) fn backup_before_upgrade(conn: &Connection, path: &Path) -> Result<Option<PathBuf>, Error> {
    let version = schema::current_version(conn)?;
    if version <= 0 || version >= schema::SCHEMA_VERSION {
        return Ok(None);
    }
    let backup = schema_backup_path(path, version);
    // Unique per attempt: two openers in one process share a pid, and
    // `VACUUM INTO` refuses a file that already has content.
    let partial = suffixed(&backup, &format!(".partial-{}", crate::new_id()));
    let Some(partial_text) = partial.to_str() else {
        return Err(Error::Invalid(format!("database path {} is not UTF-8", partial.display())));
    };
    conn.execute("VACUUM INTO ?1", [partial_text])?;
    let copied_version = match version_of_file(&partial) {
        Ok(copied_version) => copied_version,
        Err(error) => {
            remove_if_present(&partial)?;
            return Err(error);
        }
    };
    if copied_version != version {
        assert!(copied_version > version, "the snapshot went back from {version} to {copied_version}");
        remove_if_present(&partial)?;
        return Ok(None);
    }
    std::fs::rename(&partial, &backup).map_err(|source| io(&partial, source))?;
    prune_schema_backups(path)?;
    assert!(exists(&backup)?, "the newest copy is never pruned");
    Ok(Some(backup))
}

/// Delete all but the newest [`SCHEMA_BACKUPS_KEPT_MAX`] schema copies of the
/// database at `path`.  Only names [`schema_backup_path`] produces are
/// touched; other `.bak` files in the directory are the owner's.
fn prune_schema_backups(path: &Path) -> Result<(), Error> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let entries = std::fs::read_dir(directory).map_err(|source| io(directory, source))?;
    let mut versions: Vec<i64> = Vec::new();
    for entry in entries.take(DIRECTORY_ENTRIES_MAX) {
        let entry = entry.map_err(|source| io(directory, source))?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some(version) = backup_version(name, &file_name) {
            versions.push(version);
        }
    }
    versions.sort_unstable_by(|a, b| b.cmp(a));
    for version in versions.iter().skip(SCHEMA_BACKUPS_KEPT_MAX) {
        remove_if_present(&schema_backup_path(path, *version))?;
    }
    Ok(())
}

/// The schema version in `file_name` when it is a copy of `database_name`
/// (`axel.db.v7.bak` → 7).
fn backup_version(database_name: &str, file_name: &str) -> Option<i64> {
    let digits = file_name
        .strip_prefix(database_name)?
        .strip_prefix(".v")?
        .strip_suffix(".bak")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|version| *version > 0)
}

fn version_of_file(path: &Path) -> Result<i64, Error> {
    let copy = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    schema::current_version(&copy)
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_owned();
    text.push(suffix);
    PathBuf::from(text)
}

fn exists(path: &Path) -> Result<bool, Error> {
    path.try_exists().map_err(|source| io(path, source))
}

fn remove_if_present(path: &Path) -> Result<(), Error> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io(path, source)),
    }
}

fn io(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}
