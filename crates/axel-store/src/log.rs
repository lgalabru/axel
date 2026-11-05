//! The event log: `append`, authorization, projection replay and rebuild.
//!
//! Every thread-scoped envelope is projected by replaying *all* of its
//! thread's events in `(created_at, id)` order into freshly cleared projection
//! rows.  Replay is therefore deterministic and independent of arrival order:
//! two replicas holding the same set of envelopes have identical projections.
//! Scalars end up last-writer-wins, set elements (projects, participants) are
//! add/remove LWW, and usage is a sum of deltas.  Rejected envelopes stay in
//! the log and are re-evaluated on every replay, so a later participant-add
//! revives them without a manual rebuild.

use std::collections::{BTreeMap, HashMap, HashSet};

use axel_identity::{AuthTag, Delegation, PublicKey, verify_attestation};
use axel_inbox::Envelope;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::schema::PROJECTION_TABLES;
use crate::wire::{self, kind, tag};
use crate::{
    Appended, Error, ParticipantRole, SessionState, StatusState, Store,
    USAGE_SAMPLE_RETENTION_SECS, now,
};

/// Columns that reconstruct an [`Envelope`], in [`row_envelope`] order.
pub(crate) const EVENT_COLS: &str = "id, from_pubkey, created_at, kind, tags_json, content, sig";

pub(crate) fn row_envelope(r: &Row<'_>) -> rusqlite::Result<Envelope> {
    let tags_json: String = r.get("tags_json")?;
    let tags = serde_json::from_str(&tags_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Envelope {
        id: r.get("id")?,
        from: r.get("from_pubkey")?,
        created_at: r.get::<_, i64>("created_at")? as u64,
        kind: r.get::<_, i64>("kind")? as u32,
        tags,
        content: r.get("content")?,
        sig: r.get("sig")?,
    })
}

/// The thread an envelope belongs to: itself for a root, else its first `t` tag.
pub(crate) fn thread_id_of(env: &Envelope) -> Option<String> {
    if env.kind == kind::THREAD_CREATE {
        Some(env.id.clone())
    } else {
        env.thread_id().map(str::to_string)
    }
}

/// Whether an envelope is subject to thread authorization.
fn is_scoped(env: &Envelope) -> bool {
    kind::is_thread_kind(env.kind) || env.thread_id().is_some()
}

const DELEGATED_WITHOUT_THREAD: &str = "a delegated envelope must be on a thread";
const NO_THREAD_TAG: &str = "thread-scoped envelope without a [\"t\", thread_id] tag";

impl Store {
    /// Verify `env`'s id and signature, store it (idempotent on id), decide
    /// whether its signer may act on its thread, and project it.  Envelopes
    /// that fail verification are not stored (`Error::Envelope`); envelopes
    /// that fail authorization are stored with `rejected_reason` and revisited
    /// on later replays.
    pub fn append(&self, env: &Envelope) -> Result<Appended, Error> {
        env.verify()?;
        let tx = self.conn.unchecked_transaction()?;
        let appended = append_in(&tx, env)?;
        tx.commit()?;
        Ok(appended)
    }

    /// [`Store::append`] for a message built elsewhere, failing loudly when it
    /// is not projected.
    pub fn post(&self, env: &Envelope) -> Result<(), Error> {
        match self.append(env)?.rejected_reason {
            Some(reason) => Err(Error::Unauthorized(reason)),
            None => Ok(()),
        }
    }

    /// Envelopes with `seq > cursor`, oldest first, including rejected ones
    /// (sync must carry everything).  The last `seq` is the next cursor.
    pub fn events_since(&self, cursor: i64, limit: usize) -> Result<Vec<(i64, Envelope)>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT seq, {EVENT_COLS} FROM events WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2"
        ))?;
        let rows = st.query_map(params![cursor, limit as i64], |r| {
            Ok((r.get::<_, i64>("seq")?, row_envelope(r)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every envelope of a thread (projected or rejected) in replay order.
    pub fn thread_events(&self, thread_id: &str) -> Result<Vec<Envelope>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {EVENT_COLS} FROM events WHERE thread_id = ?1 ORDER BY created_at ASC, id ASC"
        ))?;
        let rows = st.query_map([thread_id], row_envelope)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// One envelope by id, whatever its projection state.
    pub fn get_event(&self, id: &str) -> Result<Option<Envelope>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {EVENT_COLS} FROM events WHERE id = ?1"),
                [id],
                row_envelope,
            )
            .optional()?)
    }

    /// Why an envelope is not projected, if it is stored and rejected.
    pub fn rejected_reason(&self, id: &str) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT rejected_reason FROM events WHERE id = ?1",
                [id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Truncate every projection table and replay the whole log.  Local-only
    /// `sessions.transcript_offset` values survive.
    pub fn rebuild_projections(&self) -> Result<(), Error> {
        let tx = self.conn.unchecked_transaction()?;
        let offsets = snapshot_offsets(&tx, None)?;
        for table in PROJECTION_TABLES {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        tx.execute(
            "UPDATE events SET applied_at = NULL, rejected_reason = NULL",
            [],
        )?;

        let plain: Vec<Envelope> = {
            let mut st = tx.prepare(&format!(
                "SELECT {EVENT_COLS} FROM events WHERE thread_id IS NULL ORDER BY seq ASC"
            ))?;
            let rows = st.query_map([], row_envelope)?;
            rows.collect::<Result<_, _>>()?
        };
        for env in &plain {
            if is_scoped(env) {
                mark_rejected(&tx, &env.id, NO_THREAD_TAG)?;
            } else if has_delegation(env) {
                // These have no thread tag, and a delegation is only valid on one.
                mark_rejected(&tx, &env.id, DELEGATED_WITHOUT_THREAD)?;
            } else {
                project_plain(&tx, env)?;
            }
        }

        let threads: Vec<String> = {
            let mut st =
                tx.prepare("SELECT DISTINCT thread_id FROM events WHERE thread_id IS NOT NULL")?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for thread_id in &threads {
            replay_thread(&tx, thread_id, Some(&offsets))?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn append_in(conn: &Connection, env: &Envelope) -> Result<Appended, Error> {
    let tags_json = serde_json::to_string(&env.tags)?;
    let thread_id = thread_id_of(env);
    let session_id = env.first_tag(tag::SESSION);
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO events (id, from_pubkey, created_at, kind, tags_json, content, sig, thread_id, session_id, received_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            env.id,
            env.from,
            env.created_at as i64,
            env.kind as i64,
            tags_json,
            env.content,
            env.sig,
            thread_id,
            session_id,
            now()
        ],
    )?;
    if inserted == 0 {
        let (projected, rejected_reason) = event_state(conn, &env.id)?;
        return Ok(Appended {
            inserted: false,
            projected,
            rejected_reason,
        });
    }
    if is_scoped(env) {
        match &thread_id {
            Some(t) => replay_thread(conn, t, None)?,
            None => mark_rejected(conn, &env.id, NO_THREAD_TAG)?,
        }
    } else if has_delegation(env) && thread_id.is_none() {
        mark_rejected(conn, &env.id, DELEGATED_WITHOUT_THREAD)?;
    } else if let Some(r) = thread_id.as_deref().and_then(|t| delegation_problem(env, t)) {
        mark_rejected(conn, &env.id, &r)?;
    } else {
        project_plain(conn, env)?;
    }
    let (projected, rejected_reason) = event_state(conn, &env.id)?;
    Ok(Appended {
        inserted: true,
        projected,
        rejected_reason,
    })
}

fn event_state(conn: &Connection, id: &str) -> Result<(bool, Option<String>), Error> {
    Ok(conn.query_row(
        "SELECT applied_at IS NOT NULL, rejected_reason FROM events WHERE id = ?1",
        [id],
        |r| Ok((r.get::<_, bool>(0)?, r.get::<_, Option<String>>(1)?)),
    )?)
}

fn mark_applied(conn: &Connection, id: &str) -> Result<(), Error> {
    conn.execute(
        "UPDATE events SET applied_at = ?2, rejected_reason = NULL WHERE id = ?1",
        params![id, now()],
    )?;
    Ok(())
}

fn mark_rejected(conn: &Connection, id: &str, reason: &str) -> Result<(), Error> {
    conn.execute(
        "UPDATE events SET applied_at = NULL, rejected_reason = ?2 WHERE id = ?1",
        params![id, reason],
    )?;
    Ok(())
}

/// The local-only session columns (`transcript_offset`, `socket_path`) are in
/// no envelope, so they are saved before a replay wipes the rows and restored
/// afterwards.
type LocalSessionState = HashMap<String, (i64, Option<String>)>;

fn snapshot_offsets(
    conn: &Connection,
    thread_id: Option<&str>,
) -> Result<LocalSessionState, Error> {
    let mut out = HashMap::new();
    let mut st = conn.prepare(
        "SELECT id, transcript_offset, socket_path FROM sessions
         WHERE (?1 IS NULL OR thread_id = ?1) AND (transcript_offset <> 0 OR socket_path IS NOT NULL)",
    )?;
    let rows = st.query_map([thread_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            (r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?),
        ))
    })?;
    for row in rows {
        let (id, local) = row?;
        out.insert(id, local);
    }
    Ok(out)
}

/// Mentions and refs of a non-thread envelope (a note, a request, ...).
fn project_plain(conn: &Connection, env: &Envelope) -> Result<(), Error> {
    index_tags(conn, env)?;
    mark_applied(conn, &env.id)
}

/// `mentions` rows for addressed kinds, `message_refs` rows for every `e` tag.
fn index_tags(conn: &Connection, env: &Envelope) -> Result<(), Error> {
    if kind::is_addressed_kind(env.kind) {
        let mut st = conn.prepare_cached(
            "INSERT OR IGNORE INTO mentions (message_id, pubkey) VALUES (?1, ?2)",
        )?;
        for pk in env.tag_values(tag::MENTION) {
            if !pk.is_empty() {
                st.execute(params![env.id, pk])?;
            }
        }
    }
    let mut st = conn.prepare_cached(
        "INSERT OR IGNORE INTO message_refs (message_id, ref_id) VALUES (?1, ?2)",
    )?;
    for id in env.tag_values(tag::REF) {
        if !id.is_empty() {
            st.execute(params![env.id, id])?;
        }
    }
    Ok(())
}

/// Pass-1 view of a participant: only the role matters for authorization;
/// the full row is written by `apply`.
#[derive(Clone, Copy, Debug)]
struct PartRow {
    role: ParticipantRole,
}

/// This replica's registered agents and their (verified) owner attestations.
fn load_own_agents(conn: &Connection) -> Result<HashMap<String, Option<AuthTag>>, Error> {
    let mut out = HashMap::new();
    let mut st =
        conn.prepare("SELECT pubkey, auth_tag_json FROM agents WHERE archived_at IS NULL")?;
    let rows = st.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
        let (pubkey, auth_json) = row?;
        let tag = auth_json
            .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
            .and_then(|t| AuthTag::from_tag(&t).ok())
            .filter(|t| {
                PublicKey::from_base58(&pubkey).is_ok_and(|pk| verify_attestation(t, &pk).is_ok())
            });
        out.insert(pubkey, tag);
    }
    Ok(out)
}

/// Who an envelope speaks for: the delegating agent when it carries a
/// `deleg` tag, else its signer.  Delegations are verified at ingest
/// ([`delegation_problem`]); an envelope whose delegation fails is rejected,
/// so a projected envelope's author can be trusted.
pub fn author(env: &Envelope) -> &str {
    env.tags
        .iter()
        .find(|t| t.len() == 5 && t[0] == axel_identity::delegate::TAG)
        .map(|t| t[1].as_str())
        .unwrap_or(&env.from)
}

fn has_delegation(env: &Envelope) -> bool {
    env.tags.iter().any(|t| t.first().map(String::as_str) == Some(axel_identity::delegate::TAG))
}

/// Why `env`'s delegation can't be honoured on `thread_id`, if it has one.
fn delegation_problem(env: &Envelope, thread_id: &str) -> Option<String> {
    let tags: Vec<&Vec<String>> =
        env.tags.iter().filter(|t| t.first().map(String::as_str) == Some(axel_identity::delegate::TAG)).collect();
    match tags.as_slice() {
        [] => None,
        [t] => {
            if matches!(env.kind, kind::THREAD_CREATE..=kind::THREAD_PARTICIPANT) {
                return Some("a delegate can't change the thread itself".into());
            }
            let check = || -> Result<(), axel_identity::Error> {
                let d = Delegation::from_tag(t)?;
                d.verify_for(&PublicKey::from_base58(&env.from)?, thread_id, env.kind, env.created_at)
            };
            check().err().map(|e| format!("invalid delegation: {e}"))
        }
        _ => Some("more than one delegation".into()),
    }
}

/// The owner that vouches for `env.from`, from an `auth` tag carried in the
/// envelope or from this replica's agent registry, if the attestation verifies
/// and its conditions permit this envelope.
fn attested_owner(env: &Envelope, own_agents: &HashMap<String, Option<AuthTag>>) -> Option<String> {
    let agent = PublicKey::from_base58(author(env)).ok()?;
    let permits = |t: &AuthTag| {
        verify_attestation(t, &agent).is_ok()
            && t.permits(env.kind, env.created_at).unwrap_or(false)
    };
    for t in env
        .tags
        .iter()
        .filter(|t| t.first().map(String::as_str) == Some(tag::AUTH))
    {
        if let Ok(auth) = AuthTag::from_tag(t)
            && permits(&auth)
        {
            return Some(auth.owner.to_base58());
        }
    }
    own_agents
        .get(author(env))
        .and_then(|t| t.as_ref())
        .filter(|t| permits(t))
        .map(|t| t.owner.to_base58())
}

/// Why `env` may not be projected on this thread, or `None` when it may.
fn authorize(
    env: &Envelope,
    participants: &BTreeMap<String, PartRow>,
    own_agents: &HashMap<String, Option<AuthTag>>,
) -> Option<String> {
    let who = author(env);
    if participants.contains_key(who) {
        return None;
    }
    if let Some(owner) = attested_owner(env, own_agents)
        && participants.contains_key(&owner)
    {
        return None;
    }
    if kind::is_own_agent_kind(env.kind) && own_agents.contains_key(who) {
        return None;
    }
    Some(format!(
        "signer {} is not a participant of this thread (kind {})",
        who, env.kind
    ))
}

/// Re-project one thread from scratch.  `offsets` carries saved
/// `transcript_offset`s when the caller already wiped `sessions`.
pub(crate) fn replay_thread(
    conn: &Connection,
    thread_id: &str,
    offsets: Option<&LocalSessionState>,
) -> Result<(), Error> {
    let events: Vec<Envelope> = {
        let mut st = conn.prepare_cached(&format!(
            "SELECT {EVENT_COLS} FROM events WHERE thread_id = ?1 ORDER BY created_at ASC, id ASC"
        ))?;
        let rows = st.query_map([thread_id], row_envelope)?;
        rows.collect::<Result<_, _>>()?
    };
    let saved_offsets = match offsets {
        Some(o) => o.clone(),
        None => snapshot_offsets(conn, Some(thread_id))?,
    };
    clear_thread(conn, thread_id)?;
    let own_agents = load_own_agents(conn)?;

    // Pass 1: the participant set, from the root and owner-signed changes.
    let mut participants: BTreeMap<String, PartRow> = BTreeMap::new();
    let mut valid_changes: HashSet<&str> = HashSet::new();
    let mut root_seen = false;
    for e in &events {
        if e.kind == kind::THREAD_CREATE && e.id == thread_id {
            participants.insert(
                e.from.clone(),
                PartRow {
                    role: ParticipantRole::Owner,
                },
            );
            root_seen = true;
        } else if e.kind == kind::THREAD_PARTICIPANT {
            let Some(target) = e.first_tag(tag::MENTION) else {
                continue;
            };
            let Ok(op) = serde_json::from_str::<wire::ParticipantOp>(&e.content) else {
                continue;
            };
            let signer_is_owner = participants
                .get(&e.from)
                .is_some_and(|p| p.role == ParticipantRole::Owner);
            let self_remove = op.op == "remove" && target == e.from;
            if !(signer_is_owner || self_remove) {
                continue;
            }
            match op.op.as_str() {
                "add" => {
                    participants.insert(
                        target.to_string(),
                        PartRow {
                            role: op.role.unwrap_or(ParticipantRole::Member),
                        },
                    );
                }
                "remove" => {
                    participants.remove(target);
                }
                _ => continue,
            }
            valid_changes.insert(e.id.as_str());
        }
    }
    if !root_seen {
        for e in &events {
            mark_rejected(conn, &e.id, "thread root (kind 100) not received yet")?;
        }
        return Ok(());
    }

    // Pass 2: authorize and apply in order.
    for e in &events {
        let reason = if e.kind == kind::THREAD_CREATE {
            (e.id != thread_id).then(|| "a second thread root on this thread".to_string())
        } else if e.kind == kind::THREAD_PARTICIPANT {
            (!valid_changes.contains(e.id.as_str()))
                .then(|| "participant change not signed by a current owner".to_string())
        } else {
            delegation_problem(e, thread_id).or_else(|| authorize(e, &participants, &own_agents))
        };
        match reason {
            Some(r) => mark_rejected(conn, &e.id, &r)?,
            None => match apply(conn, e, thread_id)? {
                Applied::Ok => mark_applied(conn, &e.id)?,
                Applied::Rejected(r) => mark_rejected(conn, &e.id, &r)?,
            },
        }
    }

    let mut st = conn.prepare_cached(
        "UPDATE sessions SET transcript_offset = ?2, socket_path = ?3 WHERE id = ?1",
    )?;
    for (id, (off, sock)) in &saved_offsets {
        st.execute(params![id, off, sock])?;
    }
    Ok(())
}

/// Whether `usage_log` exists yet: an older migration replaying threads
/// runs before the one that adds it.
fn has_usage_log(conn: &Connection) -> Result<bool, Error> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'usage_log'")?
        .exists([])?)
}

fn clear_thread(conn: &Connection, thread_id: &str) -> Result<(), Error> {
    if has_usage_log(conn)? {
        conn.execute("DELETE FROM usage_log WHERE thread_id = ?1", [thread_id])?;
    }
    for table in [
        "usage_samples",
        "usage",
        "status",
        "sessions",
        "thread_participants",
        "thread_projects",
        "turns",
    ] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE thread_id = ?1"),
            [thread_id],
        )?;
    }
    conn.execute("DELETE FROM threads WHERE id = ?1", [thread_id])?;
    conn.execute(
        "DELETE FROM mentions WHERE message_id IN (SELECT id FROM events WHERE thread_id = ?1)",
        [thread_id],
    )?;
    conn.execute(
        "DELETE FROM message_refs WHERE message_id IN (SELECT id FROM events WHERE thread_id = ?1)",
        [thread_id],
    )?;
    Ok(())
}

enum Applied {
    Ok,
    Rejected(String),
}

fn parse<T: serde::de::DeserializeOwned>(env: &Envelope) -> Result<T, Applied> {
    serde_json::from_str(&env.content)
        .map_err(|e| Applied::Rejected(format!("malformed content for kind {}: {e}", env.kind)))
}

/// Apply one authorized envelope to the projections.
fn apply(conn: &Connection, e: &Envelope, thread_id: &str) -> Result<Applied, Error> {
    let at = e.created_at as i64;
    match e.kind {
        kind::THREAD_CREATE => {
            let title = e.first_tag(tag::TITLE).unwrap_or("");
            let parent = e.tags.iter().find(|t| t.len() >= 2 && t[0] == tag::PARENT);
            let parent_id = parent.map(|t| t[1].as_str());
            let relationship = parent.and_then(|t| t.get(2)).map(String::as_str);
            conn.execute(
                "INSERT OR REPLACE INTO threads (id, title, intent, state, created_by, parent_thread_id, relationship, created_at, updated_at, done_at)
                 VALUES (?1, ?2, ?3, 'open', ?4, ?5, ?6, ?7, ?7, NULL)",
                params![e.id, title.trim(), e.content, e.from, parent_id, relationship, at],
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO thread_participants (thread_id, pubkey, role, added_by, added_at) VALUES (?1, ?2, 'owner', ?2, ?3)",
                params![e.id, e.from, at],
            )?;
        }
        kind::THREAD_UPDATE => {
            let u: wire::ThreadUpdate = match parse(e) {
                Ok(u) => u,
                Err(r) => return Ok(r),
            };
            conn.execute(
                "UPDATE threads SET
                    title = COALESCE(?2, title),
                    state = COALESCE(?3, state),
                    done_at = CASE COALESCE(?3, state)
                                WHEN 'done' THEN COALESCE(done_at, ?4)
                                WHEN 'archived' THEN done_at
                                WHEN 'deleted' THEN done_at
                                ELSE NULL END
                 WHERE id = ?1",
                params![
                    thread_id,
                    u.title.as_deref().map(str::trim),
                    u.state.map(|s| s.as_str()),
                    at
                ],
            )?;
        }
        kind::THREAD_PROJECT => {
            let op: wire::ThreadProjectOp = match parse(e) {
                Ok(o) => o,
                Err(r) => return Ok(r),
            };
            let Some(key) = op.key() else {
                return Ok(Applied::Rejected(
                    "thread.project without repo_key or path_hint".into(),
                ));
            };
            match op.op.as_str() {
                "attach" => {
                    conn.execute(
                        "INSERT OR REPLACE INTO thread_projects (thread_id, key, repo_key, path_hint, workspace_kind, branch, worktree_path, added_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            thread_id,
                            key,
                            op.repo_key.as_deref().filter(|k| !k.is_empty()),
                            op.path_hint.as_deref().unwrap_or(key),
                            op.workspace_kind.unwrap_or(crate::WorkspaceKind::InPlace).as_str(),
                            op.branch,
                            op.worktree_path,
                            at
                        ],
                    )?;
                }
                "detach" => {
                    conn.execute(
                        "DELETE FROM thread_projects WHERE thread_id = ?1 AND (key = ?2 OR repo_key = ?2 OR path_hint = ?2)",
                        params![thread_id, key],
                    )?;
                }
                other => {
                    return Ok(Applied::Rejected(format!(
                        "unknown thread.project op {other:?}"
                    )));
                }
            }
        }
        kind::THREAD_PARTICIPANT => {
            // Validated in pass 1; mirror the change.
            let op: wire::ParticipantOp = match parse(e) {
                Ok(o) => o,
                Err(r) => return Ok(r),
            };
            let target = e.first_tag(tag::MENTION).unwrap_or_default();
            if op.op == "add" {
                conn.execute(
                    "INSERT OR REPLACE INTO thread_participants (thread_id, pubkey, role, added_by, added_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![thread_id, target, op.role.unwrap_or(ParticipantRole::Member).as_str(), e.from, at],
                )?;
            } else {
                conn.execute(
                    "DELETE FROM thread_participants WHERE thread_id = ?1 AND pubkey = ?2",
                    params![thread_id, target],
                )?;
            }
        }
        kind::SESSION_START => {
            let Some(session_id) = e.first_tag(tag::SESSION) else {
                return Ok(Applied::Rejected("session.start without an s tag".into()));
            };
            let s: wire::SessionStart = match parse(e) {
                Ok(s) => s,
                Err(r) => return Ok(r),
            };
            let agent = e.first_tag(tag::MENTION).unwrap_or(author(e));
            conn.execute(
                "INSERT OR REPLACE INTO sessions (id, thread_id, agent_pubkey, environment_id, environment_label, project_repo_key,
                     harness, runtime_mode, resumed_from, runtime, state, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'starting', ?11)",
                params![
                    session_id,
                    thread_id,
                    agent,
                    s.environment_id,
                    s.environment_label,
                    s.project_repo_key,
                    s.harness.as_str(),
                    s.runtime_mode.as_str(),
                    s.resumed_from,
                    s.runtime.as_str(),
                    at
                ],
            )?;
        }
        kind::SESSION_UPDATE => {
            let Some(session_id) = e.first_tag(tag::SESSION) else {
                return Ok(Applied::Rejected("session.update without an s tag".into()));
            };
            let u: wire::SessionUpdateWire = match parse(e) {
                Ok(u) => u,
                Err(r) => return Ok(r),
            };
            conn.execute(
                "UPDATE sessions SET
                    harness_session_id = COALESCE(?2, harness_session_id),
                    transcript_path = COALESCE(?3, transcript_path),
                    state = CASE WHEN state = 'ended' THEN state ELSE COALESCE(?4, state) END,
                    runtime_mode = COALESCE(?6, runtime_mode)
                 WHERE id = ?1 AND thread_id = ?5",
                params![
                    session_id,
                    u.harness_session_id,
                    u.transcript_path,
                    u.state.map(|s| s.as_str()),
                    thread_id,
                    u.runtime_mode.map(|m| m.as_str())
                ],
            )?;
            if let Some(rex) = u.rex {
                conn.execute(
                    "UPDATE sessions SET rex_session_id = ?2, rex_window_id = ?3, rex_block_id = ?4 WHERE id = ?1 AND thread_id = ?5",
                    params![session_id, rex.session, rex.window, rex.block, thread_id],
                )?;
            }
        }
        kind::SESSION_END => {
            let Some(session_id) = e.first_tag(tag::SESSION) else {
                return Ok(Applied::Rejected("session.end without an s tag".into()));
            };
            let end: wire::SessionEnd = parse(e).unwrap_or_default();
            conn.execute(
                "UPDATE sessions SET state = 'ended', ended_at = ?2, ended_reason = ?3 WHERE id = ?1 AND thread_id = ?4",
                params![session_id, at, end.reason, thread_id],
            )?;
        }
        kind::TURN => {
            let Some(session_id) = e.first_tag(tag::SESSION) else {
                return Ok(Applied::Rejected("turn without an s tag".into()));
            };
            let t: crate::TurnSummary = match parse(e) {
                Ok(t) => t,
                Err(r) => return Ok(r),
            };
            conn.execute(
                "INSERT OR REPLACE INTO turns (id, thread_id, session_id, from_pubkey, created_at, ordinal, user_text,
                     assistant_text, tool_calls, files_edited, duration_ms, stop_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    e.id,
                    thread_id,
                    session_id,
                    author(e),
                    at,
                    t.ordinal,
                    t.user_text,
                    t.assistant_text,
                    t.tool_calls,
                    t.files_edited,
                    t.duration_ms,
                    t.stop_reason
                ],
            )?;
        }
        kind::STATUS => {
            let Some(session_id) = e.first_tag(tag::SESSION) else {
                return Ok(Applied::Rejected("status without an s tag".into()));
            };
            let s: crate::StatusReport = match parse(e) {
                Ok(s) => s,
                Err(r) => return Ok(r),
            };
            conn.execute(
                "INSERT OR REPLACE INTO status (session_id, thread_id, state, kind, title, msg, progress, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    session_id,
                    thread_id,
                    s.state.as_str(),
                    s.kind.map(|k| k.as_str()),
                    s.title,
                    s.msg,
                    s.progress.map(i64::from),
                    at
                ],
            )?;
            let live = match s.state {
                StatusState::Working => Some(SessionState::Running),
                StatusState::Blocked => Some(SessionState::Blocked),
                StatusState::Idle => Some(SessionState::Idle),
                StatusState::Done | StatusState::Error => None,
            };
            if let Some(live) = live {
                conn.execute(
                    "UPDATE sessions SET state = ?2 WHERE id = ?1 AND state <> 'ended'",
                    params![session_id, live.as_str()],
                )?;
            }
        }
        kind::USAGE => {
            let Some(session_id) = e.first_tag(tag::SESSION) else {
                return Ok(Applied::Rejected("usage without an s tag".into()));
            };
            let d: wire::UsageWire = match parse(e) {
                Ok(d) => d,
                Err(r) => return Ok(r),
            };
            conn.execute(
                "INSERT INTO usage (session_id, thread_id, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, turns, cost_usd_est, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (session_id) DO UPDATE SET
                    input_tokens = usage.input_tokens + excluded.input_tokens,
                    output_tokens = usage.output_tokens + excluded.output_tokens,
                    cache_read_tokens = usage.cache_read_tokens + excluded.cache_read_tokens,
                    cache_write_tokens = usage.cache_write_tokens + excluded.cache_write_tokens,
                    turns = usage.turns + excluded.turns,
                    cost_usd_est = usage.cost_usd_est + excluded.cost_usd_est,
                    updated_at = MAX(usage.updated_at, excluded.updated_at)",
                params![session_id, thread_id, d.input, d.output, d.cache_read, d.cache_write, d.turns, d.cost_usd_est, at],
            )?;
            if at >= now() - USAGE_SAMPLE_RETENTION_SECS {
                conn.execute(
                    "INSERT OR IGNORE INTO usage_samples (event_id, session_id, thread_id, at, input_tokens, output_tokens)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![e.id, session_id, thread_id, at, d.input, d.output],
                )?;
            }
            // Every delta, kept, under the agent whose session spent it.
            if has_usage_log(conn)? {
                conn.execute(
                    "INSERT OR IGNORE INTO usage_log (event_id, session_id, thread_id, agent_pubkey, at, model,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, turns, cost_usd_est)
                     VALUES (?1, ?2, ?3, COALESCE((SELECT agent_pubkey FROM sessions WHERE id = ?2), ?4), ?5, ?6,
                        ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![e.id, session_id, thread_id, e.from, at, d.model, d.input, d.output, d.cache_read, d.cache_write, d.reasoning, d.turns, d.cost_usd_est],
                )?;
            }
        }
        _ => {}
    }
    index_tags(conn, e)?;
    conn.execute(
        "UPDATE threads SET updated_at = MAX(updated_at, ?2) WHERE id = ?1",
        params![thread_id, at],
    )?;
    Ok(Applied::Ok)
}
