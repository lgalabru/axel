# ADR 0011: Opening the database never loses data: copy before upgrading, refuse what we don't understand

Status: accepted (2026-11-22)

## Context

Every `axel` process opens `~/.axel/axel.db` and migrates it forward. On
2026-11-22 a build from a feature branch (schema v12) upgraded the shared
database. The installed `main` build (v10) then failed every open with
`SchemaTooNew`. The fix someone reached for was to delete the database: every thread
and session in flight was gone, and the new empty database picked up the old
one's `-shm` file, which the running processes still had open. Nothing in the code
stopped that, and nothing made it easy to undo. Two more risks were also open:
two branches that each add a "migration 11" give databases that agree on the
number and differ in tables, and two processes opening an old database at the
same moment could both run the same migration.

## Decision

`Store::open` protects the file before it changes it:

- **Copy before upgrading.** A database below this build's version is first
  copied with `VACUUM INTO` to `axel.db.v<N>.bak` (N = the copy's version).
  If the copy fails, the upgrade doesn't happen. The newest two copies are
  kept.
- **Refuse, untouched, with a way out.** A newer database fails with a
  message that says to install the newer build, says not to delete the
  database, and names the `v<supported>` copy the newer build took.
- **Fingerprint each migration.** `schema_migrations.checksum` holds an FNV-1a
  hash of the migration's SQL (comments and whitespace ignored). A database
  whose recorded migration differs from this build's fails with
  `SchemaDiverged` and is not modified. Rows from before checksums are trusted.
- **One migration per `IMMEDIATE` transaction**, re-reading `user_version`
  under the write lock, so concurrent opens apply each step once.
- **Never adopt orphaned sidecars.** When the main file is missing, a leftover
  `-wal`/`-shm` is renamed `*.orphan-<secs>`, not read into the new database.

## Consequences

An upgrade costs one copy of the database (hundreds of MB on a busy machine,
seconds of I/O), and two copies sit in `~/.axel`. A branch build that migrates
the shared database still blocks `main` until it runs again or someone restores the
copy, but it can no longer cause silent damage. Branch builds should use their
own `AXEL_HOME`. Migrations stay append-only: editing an applied one's
statements now refuses databases that ran the old text.
