//! Schema and forward-only migrations.
//!
//! The schema version lives in SQLite's `user_version` pragma, and every applied
//! migration is also recorded in `schema_migrations` so `SELECT * FROM
//! schema_migrations` tells the story of a database file, with a fingerprint
//! of each migration's SQL so a database migrated by another branch's build
//! is refused instead of half-understood.  Migrations are append-only: never
//! edit an entry in `MIGRATIONS`, add a new one.  `Store::open` copies a
//! database aside before it upgrades it (see `crate::guard`).
//!
//! Tables fall in three groups (docs/DESIGN.md "Multiplayer"):
//! - **the log**: `events`, every envelope ever received;
//! - **projections**, rebuildable from the log at any time: `threads`,
//!   `thread_projects`, `thread_participants`, `sessions`, `status`, `usage`,
//!   `usage_samples`, `usage_log`, `mentions`, `message_refs`, `schedules`,
//!   `schedule_runs`;
//! - **owner records**, projected from kinds 120-122 but also written locally:
//!   `devices`, `agent_profiles` (RFD 0004);
//! - **local-only**: `environments`, `projects`, `agents`, `read_frontiers`,
//!   `session_context`, `plan_limits`, `turn_marks`, `native_usage_sources`,
//!   `native_usage`, `model_requests`, `inspected_threads`,
//!   plus the `sessions.transcript_offset` column.

use rusqlite::Connection;

use crate::Error;

/// The schema version this build of the crate produces.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Projection tables, in an order that is safe to truncate.
pub(crate) const PROJECTION_TABLES: &[&str] = &[
    "usage_log",
    "usage_samples",
    "usage",
    "status",
    "sessions",
    "thread_participants",
    "thread_projects",
    "turns",
    "threads",
    "mentions",
    "message_refs",
    "schedules",
    "schedule_runs",
];

/// Each entry is one migration: `MIGRATIONS[n]` moves a database from version
/// `n` to `n + 1`.  Each runs inside one transaction.
const MIGRATIONS: &[&str] = &[
    // v1: event log + projections + local registries.
    r#"
    CREATE TABLE schema_migrations (
        version     INTEGER PRIMARY KEY,
        applied_at  INTEGER NOT NULL
    );

    -- ---------------------------------------------------------------- the log
    CREATE TABLE events (
        seq              INTEGER PRIMARY KEY AUTOINCREMENT,
        id               TEXT NOT NULL UNIQUE,
        from_pubkey      TEXT NOT NULL,
        created_at       INTEGER NOT NULL,
        kind             INTEGER NOT NULL,
        tags_json        TEXT NOT NULL,
        content          TEXT NOT NULL,
        sig              TEXT NOT NULL,
        thread_id        TEXT,
        session_id       TEXT,
        received_at      INTEGER NOT NULL,
        applied_at       INTEGER,
        rejected_reason  TEXT
    );
    CREATE INDEX events_thread  ON events(thread_id, created_at, id);
    CREATE INDEX events_order   ON events(created_at, id);
    CREATE INDEX events_kind    ON events(kind, created_at);
    CREATE INDEX events_session ON events(session_id);

    -- ------------------------------------------------------------ projections
    CREATE TABLE threads (
        id                TEXT PRIMARY KEY,
        title             TEXT NOT NULL,
        intent            TEXT NOT NULL,
        state             TEXT NOT NULL,
        created_by        TEXT NOT NULL,
        parent_thread_id  TEXT,
        relationship      TEXT,
        created_at        INTEGER NOT NULL,
        updated_at        INTEGER NOT NULL,
        done_at           INTEGER
    );
    CREATE INDEX threads_updated ON threads(updated_at DESC);
    CREATE INDEX threads_state   ON threads(state);

    -- key = repo_key when present, else path_hint: the identity of the element
    -- for add/remove LWW.
    CREATE TABLE thread_projects (
        thread_id       TEXT NOT NULL,
        key             TEXT NOT NULL,
        repo_key        TEXT,
        path_hint       TEXT NOT NULL,
        workspace_kind  TEXT NOT NULL,
        branch          TEXT,
        worktree_path   TEXT,
        added_at        INTEGER NOT NULL,
        PRIMARY KEY (thread_id, key)
    );
    CREATE INDEX thread_projects_repo ON thread_projects(repo_key);

    CREATE TABLE thread_participants (
        thread_id  TEXT NOT NULL,
        pubkey     TEXT NOT NULL,
        role       TEXT NOT NULL,
        added_by   TEXT NOT NULL,
        added_at   INTEGER NOT NULL,
        PRIMARY KEY (thread_id, pubkey)
    );
    CREATE INDEX thread_participants_pubkey ON thread_participants(pubkey);

    CREATE TABLE sessions (
        id                   TEXT PRIMARY KEY,
        thread_id            TEXT NOT NULL,
        agent_pubkey         TEXT NOT NULL,
        environment_id       TEXT NOT NULL,
        environment_label    TEXT NOT NULL,
        project_repo_key     TEXT,
        harness              TEXT NOT NULL,
        runtime_mode         TEXT NOT NULL,
        harness_session_id   TEXT,
        transcript_path      TEXT,
        transcript_offset    INTEGER NOT NULL DEFAULT 0,   -- local-only
        resumed_from         TEXT,
        rex_session_id       TEXT,
        rex_window_id        TEXT,
        rex_block_id         TEXT,
        state                TEXT NOT NULL,
        started_at           INTEGER NOT NULL,
        ended_at             INTEGER,
        ended_reason         TEXT
    );
    CREATE INDEX sessions_thread ON sessions(thread_id, started_at DESC);
    CREATE INDEX sessions_agent  ON sessions(agent_pubkey, started_at DESC);
    CREATE INDEX sessions_env    ON sessions(environment_id);
    CREATE INDEX sessions_state  ON sessions(state);

    CREATE TABLE status (
        session_id  TEXT PRIMARY KEY,
        thread_id   TEXT NOT NULL,
        state       TEXT NOT NULL,
        kind        TEXT,
        title       TEXT,
        msg         TEXT,
        progress    INTEGER,
        updated_at  INTEGER NOT NULL
    );
    CREATE INDEX status_thread ON status(thread_id);

    CREATE TABLE usage (
        session_id          TEXT PRIMARY KEY,
        thread_id           TEXT NOT NULL,
        input_tokens        INTEGER NOT NULL DEFAULT 0,
        output_tokens       INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
        cache_write_tokens  INTEGER NOT NULL DEFAULT 0,
        turns               INTEGER NOT NULL DEFAULT 0,
        cost_usd_est        REAL    NOT NULL DEFAULT 0,
        updated_at          INTEGER NOT NULL
    );
    CREATE INDEX usage_thread ON usage(thread_id);

    CREATE TABLE usage_samples (
        event_id       TEXT PRIMARY KEY,
        session_id     TEXT NOT NULL,
        thread_id      TEXT NOT NULL,
        at             INTEGER NOT NULL,
        input_tokens   INTEGER NOT NULL,
        output_tokens  INTEGER NOT NULL
    );
    CREATE INDEX usage_samples_at      ON usage_samples(at);
    CREATE INDEX usage_samples_session ON usage_samples(session_id, at);
    CREATE INDEX usage_samples_thread  ON usage_samples(thread_id, at);

    -- One row per ["p", pubkey] tag of a projected event.
    CREATE TABLE mentions (
        message_id  TEXT NOT NULL,
        pubkey      TEXT NOT NULL,
        PRIMARY KEY (message_id, pubkey)
    );
    CREATE INDEX mentions_pubkey ON mentions(pubkey);

    -- One row per ["e", id, ...] tag of a projected event.
    CREATE TABLE message_refs (
        message_id  TEXT NOT NULL,
        ref_id      TEXT NOT NULL,
        PRIMARY KEY (message_id, ref_id)
    );
    CREATE INDEX message_refs_ref ON message_refs(ref_id);

    -- ------------------------------------------------------------- local-only
    CREATE TABLE read_frontiers (
        pubkey              TEXT NOT NULL,
        scope               TEXT NOT NULL,
        through_created_at  INTEGER NOT NULL,
        through_id          TEXT NOT NULL,
        PRIMARY KEY (pubkey, scope)
    );

    CREATE TABLE environments (
        id           TEXT PRIMARY KEY,
        label        TEXT NOT NULL,
        rex_host_id  TEXT,
        is_local     INTEGER NOT NULL DEFAULT 0,
        created_at   INTEGER NOT NULL
    );
    CREATE UNIQUE INDEX environments_local ON environments(is_local) WHERE is_local = 1;

    CREATE TABLE projects (
        id                TEXT PRIMARY KEY,
        environment_id    TEXT NOT NULL REFERENCES environments(id),
        path              TEXT NOT NULL,
        name              TEXT NOT NULL,
        repo_key          TEXT,
        axel_md_present   INTEGER NOT NULL DEFAULT 0,
        last_opened_at    INTEGER NOT NULL,
        UNIQUE (environment_id, path)
    );
    CREATE INDEX projects_recent ON projects(last_opened_at DESC);
    CREATE INDEX projects_repo   ON projects(environment_id, repo_key);

    CREATE TABLE agents (
        pubkey         TEXT PRIMARY KEY,
        name           TEXT NOT NULL UNIQUE,
        harness        TEXT NOT NULL,
        model          TEXT,
        skills_json    TEXT NOT NULL DEFAULT '[]',
        prompt         TEXT,
        color          TEXT,
        runtime_mode   TEXT NOT NULL DEFAULT 'autopilot',
        auth_tag_json  TEXT,
        created_at     INTEGER NOT NULL,
        last_seen_at   INTEGER,
        archived_at    INTEGER
    );
    "#,
    // v2: managed runtime (ACP).  Sessions learn how they run and where their
    // control socket is (columns added by `post_migrate`, which checks they
    // are not there yet); shared turn summaries get a projection; the local
    // conversation gets a table.  Everything is idempotent because an interim
    // build shipped these objects under user_version 1.  `post_migrate` then
    // projects kind-40 events that were stored before `turns` existed.
    r#"
    -- One row per kind-40 "turn" envelope: what remote participants see of a conversation.
    CREATE TABLE IF NOT EXISTS turns (
        id              TEXT PRIMARY KEY,
        thread_id       TEXT NOT NULL,
        session_id      TEXT NOT NULL,
        from_pubkey     TEXT NOT NULL,
        created_at      INTEGER NOT NULL,
        ordinal         INTEGER NOT NULL,
        user_text       TEXT NOT NULL,
        assistant_text  TEXT NOT NULL,
        tool_calls      INTEGER NOT NULL DEFAULT 0,
        files_edited    INTEGER NOT NULL DEFAULT 0,
        duration_ms     INTEGER NOT NULL DEFAULT 0,
        stop_reason     TEXT NOT NULL DEFAULT ''
    );
    CREATE INDEX IF NOT EXISTS turns_thread ON turns(thread_id, created_at, id);

    -- The managed runtime's conversation, per session.  Local-only: never leaves the machine.
    CREATE TABLE IF NOT EXISTS conversation_items (
        id            TEXT PRIMARY KEY,
        session_id    TEXT NOT NULL,
        thread_id     TEXT NOT NULL,
        seq           INTEGER NOT NULL,
        turn          INTEGER NOT NULL,
        at            INTEGER NOT NULL,          -- unix milliseconds
        kind          TEXT NOT NULL,
        status        TEXT,
        ref_id        TEXT,
        content_json  TEXT NOT NULL,
        UNIQUE (session_id, seq)
    );
    CREATE INDEX IF NOT EXISTS conversation_items_thread  ON conversation_items(thread_id, at, seq);
    CREATE INDEX IF NOT EXISTS conversation_items_ref     ON conversation_items(session_id, ref_id);
    CREATE INDEX IF NOT EXISTS conversation_items_pending ON conversation_items(status, kind);
    "#,
    // v3: agent policies (reach + budget) and summon chains.  Local-only.
    r#"
    CREATE TABLE IF NOT EXISTS agent_policies (
        pubkey       TEXT PRIMARY KEY,
        reach_json   TEXT NOT NULL,
        budget_json  TEXT NOT NULL,
        updated_at   INTEGER NOT NULL
    );
    -- A session started by another agent's session: its parent and chain depth.
    CREATE TABLE IF NOT EXISTS session_links (
        session_id         TEXT PRIMARY KEY,
        parent_session_id  TEXT NOT NULL,
        depth              INTEGER NOT NULL
    );
    "#,
    // v4: remote hosts.  All local-only.  Grants this replica holds to sign
    // for an agent on one thread (a host acting for a delegating agent);
    // per-peer sync cursors; threads shared with each peer; and revisions on
    // conversation items (kept by triggers, with tombstones for deletes) so
    // they can be shipped incrementally.  `post_migrate` adds the host
    // columns to `environments` and `rev` to `conversation_items`.
    r#"
    CREATE TABLE IF NOT EXISTS delegations (
        signer_pubkey     TEXT NOT NULL,
        thread_id         TEXT NOT NULL,
        delegator_pubkey  TEXT NOT NULL,
        deleg_tag_json    TEXT NOT NULL,
        auth_tag_json     TEXT,
        not_after         INTEGER NOT NULL,
        PRIMARY KEY (signer_pubkey, thread_id)
    );
    CREATE TABLE IF NOT EXISTS peer_cursors (
        peer    TEXT NOT NULL,
        stream  TEXT NOT NULL,
        cursor  INTEGER NOT NULL,
        PRIMARY KEY (peer, stream)
    );
    CREATE TABLE IF NOT EXISTS shared_threads (
        peer       TEXT NOT NULL,
        thread_id  TEXT NOT NULL,
        PRIMARY KEY (peer, thread_id)
    );
    CREATE TABLE IF NOT EXISTS item_rev (n INTEGER NOT NULL);
    INSERT INTO item_rev (n) SELECT 0 WHERE NOT EXISTS (SELECT 1 FROM item_rev);
    CREATE TABLE IF NOT EXISTS item_tombstones (
        id          TEXT PRIMARY KEY,
        session_id  TEXT NOT NULL,
        rev         INTEGER NOT NULL
    );
    "#,
    // v5: where an agent runs when nobody says (`axel agent edit --host`).
    // Local-only, like the agent's policy.
    r#"
    CREATE TABLE IF NOT EXISTS agent_hosts (
        agent_pubkey  TEXT PRIMARY KEY,
        host_id       TEXT NOT NULL
    );
    "#,
    // v6: token accounting by agent and over time.  `usage_log` projects
    // every usage envelope, with no retention, so any window (last hour,
    // last day, all time) is a sum; `post_migrate` replays the threads that
    // already hold some.  `session_context` is how full each session's
    // context window is right now: a snapshot, local-only.
    r#"
    CREATE TABLE IF NOT EXISTS usage_log (
        event_id            TEXT PRIMARY KEY,
        session_id          TEXT NOT NULL,
        thread_id           TEXT NOT NULL,
        agent_pubkey        TEXT NOT NULL,
        at                  INTEGER NOT NULL,
        model               TEXT,
        input_tokens        INTEGER NOT NULL,
        output_tokens       INTEGER NOT NULL,
        cache_read_tokens   INTEGER NOT NULL,
        cache_write_tokens  INTEGER NOT NULL,
        reasoning_tokens    INTEGER NOT NULL,
        turns               INTEGER NOT NULL,
        cost_usd_est        REAL    NOT NULL
    );
    CREATE INDEX IF NOT EXISTS usage_log_at     ON usage_log(at);
    CREATE INDEX IF NOT EXISTS usage_log_agent  ON usage_log(agent_pubkey, at);
    CREATE INDEX IF NOT EXISTS usage_log_thread ON usage_log(thread_id, at);
    CREATE TABLE IF NOT EXISTS session_context (
        session_id  TEXT PRIMARY KEY,
        used        INTEGER NOT NULL,
        size        INTEGER NOT NULL,
        updated_at  INTEGER NOT NULL
    );
    "#,
    // v7: plan limits (Claude's and Codex's five-hour and weekly windows),
    // as each harness last reported them for the account signed in here.
    // Antigravity's quota buckets are plans of their own (`agy gemini`).
    // Local-only.
    r#"
    CREATE TABLE IF NOT EXISTS plan_limits (
        harness     TEXT NOT NULL,
        window      TEXT NOT NULL,
        used_pct    REAL NOT NULL,
        resets_at   INTEGER,
        updated_at  INTEGER NOT NULL,
        PRIMARY KEY (harness, window)
    );
    "#,
    // v8: what each turn changed in the workspace (`TurnChanges` as JSON, from
    // the turn envelope's `changes`; NULL when the turn carried none).  The
    // column is added by `post_migrate`, which also adds it at v2 so the
    // re-projection there can write it.
    // Local-only `turn_marks`: where a pane session's turn stands, for the
    // Claude hooks, which run once per event and remember nothing themselves:
    // the ordinal, the `before` checkpoint while a turn is open, the files its
    // edit tools named, and the tree the previous turn ended with.
    r#"
    -- turns.changes_json: see post_migrate.
    CREATE TABLE IF NOT EXISTS turn_marks (
        session_id       TEXT PRIMARY KEY,
        ordinal          INTEGER NOT NULL,
        open             INTEGER NOT NULL DEFAULT 0,
        before_commit    TEXT,
        before_tree      TEXT,
        last_after_tree  TEXT,
        user_text        TEXT NOT NULL DEFAULT '',
        authored_json    TEXT NOT NULL DEFAULT '[]',
        tool_calls       INTEGER NOT NULL DEFAULT 0,
        started_at       INTEGER NOT NULL DEFAULT 0
    );
    "#,
    // Native log reconciliation: verified native logs override local usage estimates without altering signed history.
    r#"
    CREATE TABLE native_usage_sources (session_id TEXT PRIMARY KEY, checked_at INTEGER NOT NULL);
    CREATE TABLE native_usage (
        session_id TEXT NOT NULL, id TEXT NOT NULL, at INTEGER NOT NULL, model TEXT,
        input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
        cache_read_tokens INTEGER NOT NULL, cache_write_tokens INTEGER NOT NULL,
        reasoning_tokens INTEGER NOT NULL, cost_usd_est REAL NOT NULL,
        PRIMARY KEY(session_id, id)
    );
    CREATE INDEX native_usage_at ON native_usage(at);
    CREATE VIEW effective_usage_log AS
      SELECT event_id, session_id, thread_id, agent_pubkey, at, model,
        CASE WHEN n.session_id IS NULL THEN input_tokens ELSE 0 END AS input_tokens,
        CASE WHEN n.session_id IS NULL THEN output_tokens ELSE 0 END AS output_tokens,
        CASE WHEN n.session_id IS NULL THEN cache_read_tokens ELSE 0 END AS cache_read_tokens,
        CASE WHEN n.session_id IS NULL THEN cache_write_tokens ELSE 0 END AS cache_write_tokens,
        CASE WHEN n.session_id IS NULL THEN reasoning_tokens ELSE 0 END AS reasoning_tokens,
        turns, CASE WHEN n.session_id IS NULL THEN cost_usd_est ELSE 0 END AS cost_usd_est
      FROM usage_log u LEFT JOIN native_usage_sources n USING(session_id)
      UNION ALL
      SELECT 'native:' || n.id, n.session_id, s.thread_id, s.agent_pubkey, n.at, n.model,
        n.input_tokens, n.output_tokens, n.cache_read_tokens, n.cache_write_tokens,
        n.reasoning_tokens, 0, n.cost_usd_est
      FROM native_usage n JOIN sessions s ON s.id = n.session_id;
    CREATE VIEW effective_usage AS
      SELECT session_id, thread_id, SUM(input_tokens) AS input_tokens,
        SUM(output_tokens) AS output_tokens, SUM(cache_read_tokens) AS cache_read_tokens,
        SUM(cache_write_tokens) AS cache_write_tokens, SUM(turns) AS turns,
        SUM(cost_usd_est) AS cost_usd_est, MAX(at) AS updated_at
      FROM effective_usage_log GROUP BY session_id;
    "#,
    // v10: an agent's reasoning effort, beside its model.  NULL (every agent
    // already here) is the harness's own default.  Local-only, like the rest
    // of the agent's profile.  The column is added by `post_migrate`.
    r#"
    -- agents.effort: see post_migrate.
    "#,
    // v11: what the inspector proxy saw, one row per request a harness sent to
    // its model provider.  Local-only and metadata only: sizes, tokens,
    // timings and a context breakdown, never a body, a header or a credential.
    r#"
    CREATE TABLE model_requests (
        id                  TEXT PRIMARY KEY,
        session_id          TEXT NOT NULL,
        thread_id           TEXT NOT NULL,
        agent_pubkey        TEXT NOT NULL,
        turn                INTEGER,
        provider            TEXT NOT NULL,
        endpoint            TEXT NOT NULL,
        model               TEXT,
        started_at_ms       INTEGER NOT NULL,
        first_byte_ms       INTEGER,
        duration_ms         INTEGER,
        http_status         INTEGER,
        streamed            INTEGER NOT NULL,
        outcome             TEXT NOT NULL,
        input_tokens        INTEGER,
        output_tokens       INTEGER,
        cache_read_tokens   INTEGER,
        cache_write_tokens  INTEGER,
        reasoning_tokens    INTEGER,
        request_bytes       INTEGER NOT NULL,
        response_bytes      INTEGER NOT NULL,
        stop_reason         TEXT,
        error               TEXT,
        breakdown_json      TEXT
    );
    CREATE INDEX model_requests_at      ON model_requests(started_at_ms);
    CREATE INDEX model_requests_thread  ON model_requests(thread_id, started_at_ms);
    CREATE INDEX model_requests_session ON model_requests(session_id, started_at_ms);

    -- Threads the owner asked to inspect (`axel inspect on`): only their new
    -- managed sessions run through the proxy.
    CREATE TABLE inspected_threads (
        thread_id  TEXT PRIMARY KEY,
        since_ms   INTEGER NOT NULL
    );
    "#,
    // v12: whether the proxy throttled a request on purpose (`AXEL_THROTTLE`,
    // a testing aid), so slow or lost requests read as what they were.
    // The column is added by `post_migrate`.
    r#"
    -- model_requests.throttled: see post_migrate.
    "#,
    // v13: agents become a profile and its instances, and machines can be
    // linked to one owner (RFD 0004).  A profile is what the agent is and is
    // the same on every device of its owner; an instance is that profile on
    // one machine, with its own key.  Each existing agent becomes a profile
    // (id `p-<pubkey>`, so the migration needs no randomness) plus its one
    // instance.  `agents` is rebuilt because SQLite can't drop the old
    // `name UNIQUE`: two instances of one profile share a name.
    // `post_migrate` fills `agent_profiles.owner` from each attestation.
    r#"
    CREATE TABLE agent_profiles (
        profile_id     TEXT PRIMARY KEY,
        owner          TEXT,
        name           TEXT NOT NULL,
        harness        TEXT NOT NULL,
        model          TEXT,
        effort         TEXT,
        skills_json    TEXT NOT NULL DEFAULT '[]',
        prompt         TEXT,
        color          TEXT,
        runtime_mode   TEXT NOT NULL DEFAULT 'autopilot',
        created_at     INTEGER NOT NULL,
        updated_at     INTEGER NOT NULL,
        updated_id     TEXT,
        archived_at    INTEGER
    );
    CREATE INDEX agent_profiles_name ON agent_profiles(name);
    INSERT INTO agent_profiles (profile_id, name, harness, model, effort, skills_json, prompt, color, runtime_mode, created_at, updated_at, archived_at)
        SELECT 'p-' || pubkey, name, harness, model, effort, skills_json, prompt, color, runtime_mode, created_at, created_at, archived_at FROM agents;

    CREATE TABLE agents_v13 (
        pubkey         TEXT PRIMARY KEY,
        profile_id     TEXT NOT NULL REFERENCES agent_profiles(profile_id),
        device         TEXT,
        auth_tag_json  TEXT,
        announced_id   TEXT,
        created_at     INTEGER NOT NULL,
        last_seen_at   INTEGER,
        archived_at    INTEGER
    );
    INSERT INTO agents_v13 (pubkey, profile_id, auth_tag_json, created_at, last_seen_at, archived_at)
        SELECT pubkey, 'p-' || pubkey, auth_tag_json, created_at, last_seen_at, archived_at FROM agents;
    DROP TABLE agents;
    ALTER TABLE agents_v13 RENAME TO agents;
    CREATE INDEX agents_profile ON agents(profile_id);

    -- Machines linked to an owner, projected from kind-120 envelopes.  A row
    -- with no owner is an owner's own machine label.
    -- One row per link, so a key linked, revoked and linked again keeps
    -- the gap between.
    CREATE TABLE devices (
        link_event_id    TEXT PRIMARY KEY,
        pubkey           TEXT NOT NULL,
        owner            TEXT,
        label            TEXT NOT NULL,
        route            TEXT,
        linked_at        INTEGER NOT NULL,
        revoked_at       INTEGER,
        revoke_event_id  TEXT
    );
    CREATE INDEX devices_pubkey ON devices(pubkey, linked_at);
    "#,
    // v14: scheduled work (RFD 0006), two projections of the log: `schedules`
    // from the kind 124 envelopes, and `schedule_runs`, one row per run note
    // (tag `["sched", schedule_id, run_number]`).  The primary key is what
    // drops a second run with the same number at ingest.  Self-contained, so
    // it needs no `post_migrate`: no earlier build wrote either kind of
    // envelope, so there is nothing in the log to replay into them.
    r#"
    CREATE TABLE schedules (
        schedule_id   TEXT PRIMARY KEY,
        event_id      TEXT NOT NULL,
        author        TEXT NOT NULL,
        to_pubkey     TEXT NOT NULL,
        device        TEXT NOT NULL,
        created_at    INTEGER NOT NULL,
        cancelled_at  INTEGER,
        content       TEXT NOT NULL
    );
    CREATE INDEX schedules_device ON schedules(device, created_at);
    CREATE TABLE schedule_runs (
        schedule_id  TEXT NOT NULL,
        run_number   INTEGER NOT NULL,
        event_id     TEXT NOT NULL UNIQUE,
        fired_at     INTEGER NOT NULL,
        PRIMARY KEY (schedule_id, run_number)
    );
    "#,
    // v15: teams, a named set of an owner's agent profiles with skills and a
    // reach rule of their own (RFD 0005).  Projected from kind-123 envelopes,
    // and edited here unsigned (`updated_id` NULL) until the next publish, as
    // `agent_profiles` are.  Members are profile ids; `position` keeps the
    // order they were added in, for the roster.
    r#"
    CREATE TABLE teams (
        team_id      TEXT PRIMARY KEY,
        owner        TEXT NOT NULL,
        name         TEXT NOT NULL,
        skills_json  TEXT NOT NULL DEFAULT '[]',
        reach        TEXT NOT NULL DEFAULT 'none',
        lead         TEXT,
        created_at   INTEGER NOT NULL,
        updated_at   INTEGER NOT NULL,
        updated_id   TEXT,
        archived_at  INTEGER
    );
    CREATE INDEX teams_name ON teams(name);

    CREATE TABLE team_members (
        team_id     TEXT NOT NULL REFERENCES teams(team_id) ON DELETE CASCADE,
        profile_id  TEXT NOT NULL,
        role        TEXT NOT NULL DEFAULT '',
        position    INTEGER NOT NULL,
        PRIMARY KEY (team_id, profile_id)
    );
    CREATE INDEX team_members_profile ON team_members(profile_id);
    "#,
    // v16: a profile merged into another (RFD 0004 phase 7, two homes that
    // each had a `reviewer`).  Projected from the kind-122 field of the same
    // name; a merged profile is archived and its instances belong to
    // `merged_into`.  NULL everywhere before: nothing was merged.
    r#"
    ALTER TABLE agent_profiles ADD COLUMN merged_into TEXT;
    "#,
];

/// The SQL of migration `version` (1-based), for tests that stage a database by hand.
#[cfg(test)]
pub(crate) fn migration_sql(version: i64) -> &'static str {
    MIGRATIONS[(version - 1) as usize]
}

/// `ALTER TABLE … ADD COLUMN` unless the column is already there.
fn add_column_if_missing(
    tx: &Connection,
    table: &str,
    column: &str,
    ddl: &str,
) -> Result<(), Error> {
    if !has_column(tx, table, column)? {
        tx.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"),
            [],
        )?;
    }
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, Error> {
    let columns = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>("name"))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(columns.iter().any(|c| c == column))
}

/// Rust work that a migration needs after its SQL, inside the same transaction.
fn post_migrate(tx: &Connection, version: i64) -> Result<(), Error> {
    match version {
        2 => post_migrate_v2(tx),
        4 => post_migrate_v4(tx),
        // Re-project the threads that already hold usage, so `usage_log`
        // reflects the log.
        6 => replay_threads_holding(tx, crate::kind::USAGE),
        8 => add_column_if_missing(tx, "turns", "changes_json", "TEXT"),
        10 => add_column_if_missing(tx, "agents", "effort", "TEXT"),
        12 => add_column_if_missing(tx, "model_requests", "throttled", "INTEGER NOT NULL DEFAULT 0"),
        13 => crate::owner::fill_profile_owners(tx),
        _ => Ok(()),
    }
}

/// Replay every thread that holds at least one event of `kind`.
fn replay_threads_holding(tx: &Connection, kind: u32) -> Result<(), Error> {
    let threads: Vec<String> = {
        let mut statement = tx.prepare(
            "SELECT DISTINCT thread_id FROM events WHERE kind = ?1 AND thread_id IS NOT NULL",
        )?;
        let rows = statement.query_map([kind], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for thread_id in &threads {
        crate::log::replay_thread(tx, thread_id, None)?;
    }
    Ok(())
}

fn post_migrate_v2(tx: &Connection) -> Result<(), Error> {
    add_column_if_missing(tx, "sessions", "runtime", "TEXT NOT NULL DEFAULT 'pane'")?;
    add_column_if_missing(tx, "sessions", "socket_path", "TEXT")?;
    // The projection below writes every column the current code knows.
    add_column_if_missing(tx, "turns", "changes_json", "TEXT")?;
    // Re-project every thread that already holds a turn envelope, so the
    // new `turns` table reflects the log.  (Older `sessions` rows keep
    // the default `runtime = 'pane'`.)
    replay_threads_holding(tx, crate::kind::TURN)
}

fn post_migrate_v4(tx: &Connection) -> Result<(), Error> {
    add_column_if_missing(tx, "environments", "ssh_target", "TEXT")?;
    add_column_if_missing(tx, "environments", "host_pubkey", "TEXT")?;
    add_column_if_missing(tx, "conversation_items", "rev", "INTEGER NOT NULL DEFAULT 0")?;
    tx.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS conversation_items_rev ON conversation_items(rev);
        CREATE TRIGGER IF NOT EXISTS conversation_items_rev_ins AFTER INSERT ON conversation_items BEGIN
            UPDATE item_rev SET n = n + 1;
            UPDATE conversation_items SET rev = (SELECT n FROM item_rev) WHERE id = NEW.id;
        END;
        CREATE TRIGGER IF NOT EXISTS conversation_items_rev_upd AFTER UPDATE ON conversation_items
            WHEN NEW.rev = OLD.rev BEGIN
            UPDATE item_rev SET n = n + 1;
            UPDATE conversation_items SET rev = (SELECT n FROM item_rev) WHERE id = NEW.id;
        END;
        CREATE TRIGGER IF NOT EXISTS conversation_items_rev_del AFTER DELETE ON conversation_items BEGIN
            UPDATE item_rev SET n = n + 1;
            INSERT OR REPLACE INTO item_tombstones (id, session_id, rev) VALUES (OLD.id, OLD.session_id, (SELECT n FROM item_rev));
        END;
        UPDATE item_rev SET n = (SELECT COUNT(*) FROM conversation_items);
        "#,
    )?;
    // Number the items already here.  The update trigger only fires when
    // `rev` itself is unchanged, so setting it directly doesn't recurse.
    tx.execute("UPDATE conversation_items SET rev = rowid", [])?;
    tx.execute("UPDATE item_rev SET n = (SELECT COALESCE(MAX(rev), 0) FROM conversation_items)", [])?;
    Ok(())
}

/// Read the schema version of an open connection.
pub fn current_version(conn: &Connection) -> Result<i64, Error> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?)
}

/// Bring the database up to [`SCHEMA_VERSION`].  Idempotent: a database that is
/// already current is left untouched.  A database from a newer build fails with
/// [`Error::SchemaTooNew`], and one whose applied migrations differ from this
/// build's fails with [`Error::SchemaDiverged`]; neither is modified.
pub fn migrate(conn: &mut Connection) -> Result<(), Error> {
    migrate_to(conn, SCHEMA_VERSION)
}

/// Apply migrations up to `target` (≤ [`SCHEMA_VERSION`]).  Tests use it to
/// stage an older database; production code always goes to the latest.
pub(crate) fn migrate_to(conn: &mut Connection, target: i64) -> Result<(), Error> {
    assert!(target >= 0, "migration target {target} is negative");
    let goal = target.min(SCHEMA_VERSION);
    let mut version = current_version(conn)?;
    refuse_too_new(version)?;
    verify_applied(conn, version)?;
    // One step per migration at most: each pass applies one, or finds that
    // another process already did.
    for _ in 0..MIGRATIONS.len() {
        if version >= goal {
            break;
        }
        version = apply_next(conn, version)?;
    }
    assert!(version >= goal);
    debug_assert_eq!(current_version(conn)?, version);
    Ok(())
}

/// Apply the migration after `version`, unless another process got there
/// first.  Returns the version the database is at afterwards.
///
/// The transaction is `IMMEDIATE` so it holds the write lock before it reads
/// `user_version`: two processes upgrading the same file at once then run
/// each step exactly once, instead of both trying to create the same table.
fn apply_next(conn: &mut Connection, version: i64) -> Result<i64, Error> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let found = current_version(&tx)?;
    if found != version {
        // Another process migrated while we waited for the lock.
        assert!(found > version, "schema version went back from {version} to {found}");
        refuse_too_new(found)?;
        tx.commit()?;
        return Ok(found);
    }
    let next = version + 1;
    let Ok(index) = usize::try_from(version) else {
        return Err(Error::Corrupt(format!("negative schema version {version}")));
    };
    tx.execute_batch(MIGRATIONS[index])?;
    post_migrate(&tx, next)?;
    add_column_if_missing(&tx, "schema_migrations", "checksum", "TEXT")?;
    tx.execute(
        "INSERT INTO schema_migrations (version, applied_at, checksum) VALUES (?1, ?2, ?3)",
        rusqlite::params![next, crate::now(), fingerprint(MIGRATIONS[index])],
    )?;
    tx.pragma_update(None, "user_version", next)?;
    tx.commit()?;
    Ok(next)
}

fn refuse_too_new(version: i64) -> Result<(), Error> {
    if version > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found: version,
            supported: SCHEMA_VERSION,
            backup: None,
        });
    }
    if version < 0 {
        return Err(Error::Corrupt(format!("negative schema version {version}")));
    }
    Ok(())
}

/// Check that every migration this database records as applied is the one
/// this build would have applied at that version.
///
/// Two branches that each add "migration 11" produce databases that agree on
/// the number and disagree on the tables.  Without this check a build from
/// one branch would open the other's database as current and fail later, in
/// whatever query first touches a table that isn't there.  Rows recorded
/// before checksums existed have none and are trusted.
fn verify_applied(conn: &Connection, version: i64) -> Result<(), Error> {
    if version == 0 {
        return Ok(());
    }
    if !has_column(conn, "schema_migrations", "checksum")? {
        return Ok(());
    }
    let mut statement = conn.prepare(
        "SELECT version, checksum FROM schema_migrations \
         WHERE checksum IS NOT NULL AND version BETWEEN 1 AND ?1 ORDER BY version",
    )?;
    let rows = statement.query_map([version], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (applied, found) = row?;
        let index = usize::try_from(applied - 1).expect("version is at least 1 by the query");
        let expected = fingerprint(MIGRATIONS[index]);
        if found != expected {
            return Err(Error::SchemaDiverged {
                version: applied,
                found,
                expected,
            });
        }
    }
    Ok(())
}

/// A short, stable fingerprint of a migration's SQL: FNV-1a (64-bit) over its
/// statements, with `--` comment lines and runs of whitespace ignored so that
/// rewording a comment never locks anyone out of their database.
pub(crate) fn fingerprint(sql: &str) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET_BASIS;
    let statements = sql.lines().filter(|line| !line.trim_start().starts_with("--"));
    for word in statements.flat_map(str::split_whitespace) {
        for byte in word.bytes().chain(std::iter::once(b' ')) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    format!("{hash:016x}")
}
