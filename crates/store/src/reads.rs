//! Reads over the projections and the inbox.

use std::path::PathBuf;

use axel_inbox::Envelope;
use rusqlite::{OptionalExtension, Row, ToSql, params, params_from_iter};

use crate::log::{EVENT_COLS, row_envelope};
use crate::wire::{self, ParticipantAction, kind, tag};
use crate::{TurnMark,
    Error, Harness, INBOX_SCOPE, Participant, ParticipantRole, Session, SessionFilter, Status, Store, Thread,
    ThreadFilter, ThreadProject, ThreadSummary, ThreadWorkspace, Turn, TurnSummary, Usage,
    UsageSample, UsageTotals, UsageWindows, SessionContext, LimitWindow, PlanLimit,
    SECONDS_PER_DAY, SECONDS_PER_HOUR, get_integer, sql_integer, sql_limit,
};

fn conversion_error<E: std::error::Error + Send + Sync + 'static>(e: E) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

// ---------------------------------------------------------------------------
// threads
// ---------------------------------------------------------------------------

const THREAD_COLS: &str = "t.id, t.title, t.intent, t.state, t.created_by, t.parent_thread_id, t.relationship, t.created_at, t.updated_at, t.done_at";

fn row_thread(r: &Row<'_>) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: r.get("id")?,
        title: r.get("title")?,
        intent: r.get("intent")?,
        state: r.get::<_, String>("state")?.parse().map_err(conversion_error)?,
        created_by: r.get("created_by")?,
        parent_thread_id: r.get("parent_thread_id")?,
        // Text this build does not know reads as "no relationship".
        relationship: r
            .get::<_, Option<String>>("relationship")?
            .and_then(|s| s.parse().ok()),
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        done_at: r.get("done_at")?,
    })
}

impl Store {
    /// One thread by id.
    pub fn get_thread(&self, id: &str) -> Result<Option<Thread>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {THREAD_COLS} FROM threads t WHERE t.id = ?1"),
                [id],
                row_thread,
            )
            .optional()?)
    }

    /// Threads matching `f`, most recently updated first.
    pub fn list_threads(&self, f: ThreadFilter) -> Result<Vec<Thread>, Error> {
        let mut sql = format!("SELECT {THREAD_COLS} FROM threads t WHERE 1 = 1");
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        if let Some(states) = f.states {
            if states.is_empty() {
                return Ok(Vec::new());
            }
            let mut placeholders = Vec::new();
            for s in states {
                args.push(Box::new(s.as_str()));
                placeholders.push(format!("?{}", args.len()));
            }
            sql.push_str(&format!(" AND t.state IN ({})", placeholders.join(", ")));
        }
        if let Some(a) = f.agent_pubkey {
            args.push(Box::new(a));
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM sessions s WHERE s.thread_id = t.id AND s.agent_pubkey = ?{})",
                args.len()
            ));
        }
        if let Some(p) = f.participant {
            args.push(Box::new(p));
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM thread_participants p WHERE p.thread_id = t.id AND p.pubkey = ?{})",
                args.len()
            ));
        }
        if let Some(k) = f.repo_key {
            args.push(Box::new(k));
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM thread_projects tp WHERE tp.thread_id = t.id AND tp.repo_key = ?{})",
                args.len()
            ));
        }
        sql.push_str(" ORDER BY t.updated_at DESC, t.created_at DESC, t.id DESC");
        if let Some(limit) = f.limit {
            args.push(Box::new(sql_limit(limit)));
            sql.push_str(&format!(" LIMIT ?{}", args.len()));
        }
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(
            params_from_iter(args.iter().map(|b| b.as_ref())),
            row_thread,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Everything the dashboard shows about one thread.
    pub fn thread_summary(&self, id: &str) -> Result<Option<ThreadSummary>, Error> {
        let Some(thread) = self.get_thread(id)? else {
            return Ok(None);
        };
        self.summarize(thread).map(Some)
    }

    /// Agents currently attached to a thread. Old sessions remain historical
    /// after removal; a later session start explicitly rejoins the agent.
    pub fn thread_agents(&self, thread_id: &str) -> Result<Vec<String>, Error> {
        let mut roster: std::collections::BTreeSet<String> = self.distinct(
            "SELECT DISTINCT agent_pubkey FROM sessions WHERE thread_id = ?1", thread_id
        )?.into_iter().collect();
        let mut statement = self.conn.prepare("SELECT kind, tags_json, content FROM events WHERE thread_id = ?1 AND kind IN (?2, ?3) AND applied_at IS NOT NULL AND rejected_reason IS NULL ORDER BY created_at, id")?;
        let events = statement.query_map(rusqlite::params![thread_id, kind::THREAD_PARTICIPANT, kind::SESSION_START], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        for event in events {
            let (event_kind, tags, content) = event?;
            let tags: Vec<Vec<String>> = serde_json::from_str(&tags)?;
            let Some(target) = tags.iter().find(|t| t.first().is_some_and(|s| s == tag::MENTION)).and_then(|t| t.get(1)) else { continue };
            if event_kind == kind::SESSION_START {
                roster.insert(target.clone());
                continue;
            }
            // Only applied changes are read, and applying one parsed it.
            let change: wire::ParticipantOp = serde_json::from_str(&content)?;
            match change.op {
                ParticipantAction::Remove => {
                    roster.remove(target);
                }
                ParticipantAction::Add => {
                    if change.role == Some(ParticipantRole::Agent) {
                        roster.insert(target.clone());
                    }
                }
            }
        }
        Ok(roster.into_iter().collect())
    }

    /// [`Store::thread_summary`] for every thread matching `f`.
    pub fn thread_summaries(&self, f: ThreadFilter) -> Result<Vec<ThreadSummary>, Error> {
        self.list_threads(f)?
            .into_iter()
            .map(|t| self.summarize(t))
            .collect()
    }

    fn summarize(&self, thread: Thread) -> Result<ThreadSummary, Error> {
        let id = thread.id.clone();
        let projects = self.thread_projects(&id)?;
        let participants = self.participants(&id)?;
        let sessions_total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE thread_id = ?1",
            [&id],
            |r| r.get(0),
        )?;
        let sessions_live = self.list_sessions(SessionFilter {
            thread_id: Some(id.clone()),
            states: Some(
                crate::SessionState::ALL
                    .iter()
                    .copied()
                    .filter(|s| *s != crate::SessionState::Ended)
                    .collect(),
            ),
            ..Default::default()
        })?;
        let agents = self.thread_agents(&id)?;
        let environments = self.distinct(
            "SELECT DISTINCT environment_label FROM sessions WHERE thread_id = ?1 ORDER BY 1",
            &id,
        )?;
        let usage = self.thread_usage(&id)?;
        let live = {
            let mut st = self.conn.prepare(&format!(
                "SELECT {STATUS_COLS} FROM status st JOIN sessions s ON s.id = st.session_id
                 WHERE st.thread_id = ?1 AND s.state <> 'ended'"
            ))?;
            let rows = st.query_map([&id], row_status)?;
            let mut all: Vec<Status> = rows.collect::<Result<_, _>>()?;
            all.sort_by(|a, b| {
                (a.urgency(), a.updated_at, &a.session_id).cmp(&(
                    b.urgency(),
                    b.updated_at,
                    &b.session_id,
                ))
            });
            all.pop()
        };
        Ok(ThreadSummary {
            thread,
            projects,
            participants,
            sessions_total,
            sessions_live,
            agents,
            environments,
            usage,
            live,
        })
    }

    fn distinct(&self, sql: &str, arg: &str) -> Result<Vec<String>, Error> {
        let mut st = self.conn.prepare(sql)?;
        let rows = st.query_map([arg], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Projects attached to a thread, oldest attachment first, resolved to
    /// this replica's `projects` rows where possible.
    pub fn thread_projects(&self, thread_id: &str) -> Result<Vec<ThreadProject>, Error> {
        let local_env = self.local_environment_id()?;
        let mut st = self.conn.prepare(
            "SELECT thread_id, repo_key, path_hint, workspace_kind, branch, worktree_path, added_at
             FROM thread_projects WHERE thread_id = ?1 ORDER BY added_at ASC, key ASC",
        )?;
        let rows = st.query_map([thread_id], |r| {
            Ok(ThreadProject {
                thread_id: r.get("thread_id")?,
                repo_key: r.get("repo_key")?,
                path_hint: PathBuf::from(r.get::<_, String>("path_hint")?),
                workspace: ThreadWorkspace {
                    kind: r
                        .get::<_, String>("workspace_kind")?
                        .parse()
                        .map_err(conversion_error)?,
                    branch: r.get("branch")?,
                    worktree_path: r
                        .get::<_, Option<String>>("worktree_path")?
                        .map(PathBuf::from),
                },
                local_project_id: None,
                added_at: r.get("added_at")?,
            })
        })?;
        let mut out: Vec<ThreadProject> = rows.collect::<Result<_, _>>()?;
        if let Some(env) = local_env {
            for project in &mut out {
                project.local_project_id = self.local_project_for(&env, project)?;
            }
        }
        Ok(out)
    }

    /// This replica's `projects.id` for an attached project: by repository
    /// first, else by its path.
    fn local_project_for(&self, env: &str, project: &ThreadProject) -> Result<Option<String>, Error> {
        if let Some(key) = &project.repo_key
            && let Some(found) = self.project_by_repo_key(env, key)?
        {
            return Ok(Some(found.id));
        }
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM projects WHERE environment_id = ?1 AND path = ?2",
                params![env, project.path_hint.to_string_lossy()],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Who may act on a thread, owners first, then by join time.
    pub fn participants(&self, thread_id: &str) -> Result<Vec<Participant>, Error> {
        let mut st = self.conn.prepare(
            "SELECT thread_id, pubkey, role, added_by, added_at FROM thread_participants
             WHERE thread_id = ?1 ORDER BY role = 'owner' DESC, added_at ASC, pubkey ASC",
        )?;
        let rows = st.query_map([thread_id], |r| {
            Ok(Participant {
                thread_id: r.get("thread_id")?,
                pubkey: r.get("pubkey")?,
                role: r.get::<_, String>("role")?.parse().map_err(conversion_error)?,
                added_by: r.get("added_by")?,
                added_at: r.get("added_at")?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

// ---------------------------------------------------------------------------
// sessions
// ---------------------------------------------------------------------------

const SESSION_COLS: &str = "s.id, s.thread_id, s.agent_pubkey, s.environment_id, s.environment_label, s.project_repo_key, s.harness, s.runtime_mode, s.harness_session_id, s.transcript_path, s.transcript_offset, s.resumed_from, s.runtime, s.socket_path, s.rex_session_id, s.rex_window_id, s.rex_block_id, s.state, s.started_at, s.ended_at, s.ended_reason";

fn row_session(r: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get("id")?,
        thread_id: r.get("thread_id")?,
        agent_pubkey: r.get("agent_pubkey")?,
        environment_id: r.get("environment_id")?,
        environment_label: r.get("environment_label")?,
        project_repo_key: r.get("project_repo_key")?,
        harness: Harness::parse(&r.get::<_, String>("harness")?),
        // Text another build wrote reads as the default mode / runtime, the
        // same leniency `wire::SessionStart` applies to the envelope.
        runtime_mode: r
            .get::<_, String>("runtime_mode")?
            .parse()
            .unwrap_or_default(),
        harness_session_id: r.get("harness_session_id")?,
        transcript_path: r.get("transcript_path")?,
        transcript_offset: r.get("transcript_offset")?,
        resumed_from: r.get("resumed_from")?,
        runtime: r.get::<_, String>("runtime")?.parse().unwrap_or_default(),
        socket_path: r
            .get::<_, Option<String>>("socket_path")?
            .map(PathBuf::from),
        rex_session_id: r.get("rex_session_id")?,
        rex_window_id: r.get("rex_window_id")?,
        rex_block_id: r.get("rex_block_id")?,
        state: r.get::<_, String>("state")?.parse().map_err(conversion_error)?,
        started_at: r.get("started_at")?,
        ended_at: r.get("ended_at")?,
        ended_reason: r.get("ended_reason")?,
    })
}

impl Store {
    /// One session by id.
    pub fn get_session(&self, id: &str) -> Result<Option<Session>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SESSION_COLS} FROM sessions s WHERE s.id = ?1"),
                [id],
                row_session,
            )
            .optional()?)
    }

    /// Sessions matching `f`, most recently started first.
    pub fn list_sessions(&self, f: SessionFilter) -> Result<Vec<Session>, Error> {
        let mut sql = format!("SELECT {SESSION_COLS} FROM sessions s WHERE 1 = 1");
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        for (col, val) in [
            ("thread_id", f.thread_id),
            ("agent_pubkey", f.agent_pubkey),
            ("environment_id", f.environment_id),
        ] {
            if let Some(v) = val {
                args.push(Box::new(v));
                sql.push_str(&format!(" AND s.{col} = ?{}", args.len()));
            }
        }
        if let Some(states) = f.states {
            if states.is_empty() {
                return Ok(Vec::new());
            }
            let mut placeholders = Vec::new();
            for s in states {
                args.push(Box::new(s.as_str()));
                placeholders.push(format!("?{}", args.len()));
            }
            sql.push_str(&format!(" AND s.state IN ({})", placeholders.join(", ")));
        }
        sql.push_str(" ORDER BY s.started_at DESC, s.id DESC");
        if let Some(limit) = f.limit {
            args.push(Box::new(sql_limit(limit)));
            sql.push_str(&format!(" LIMIT ?{}", args.len()));
        }
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(
            params_from_iter(args.iter().map(|b| b.as_ref())),
            row_session,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Turn summaries shared on a thread, oldest first.
    pub fn turns(&self, thread_id: &str) -> Result<Vec<Turn>, Error> {
        let mut st = self.conn.prepare(
            "SELECT id, thread_id, session_id, from_pubkey, created_at, ordinal, user_text, assistant_text,
                    tool_calls, files_edited, duration_ms, stop_reason, changes_json
             FROM turns WHERE thread_id = ?1 ORDER BY created_at ASC, id ASC",
        )?;
        let rows = st.query_map([thread_id], |r| {
            Ok(Turn {
                id: r.get("id")?,
                thread_id: r.get("thread_id")?,
                session_id: r.get("session_id")?,
                from_pubkey: r.get("from_pubkey")?,
                created_at: get_integer(r, "created_at")?,
                summary: TurnSummary {
                    ordinal: r.get("ordinal")?,
                    user_text: r.get("user_text")?,
                    assistant_text: r.get("assistant_text")?,
                    tool_calls: r.get("tool_calls")?,
                    files_edited: r.get("files_edited")?,
                    duration_ms: r.get("duration_ms")?,
                    stop_reason: r.get("stop_reason")?,
                    // A shape this build can't read shows as "no changes
                    // recorded" rather than hiding the whole turn.
                    changes: r.get::<_, Option<String>>("changes_json")?.and_then(|j| serde_json::from_str(&j).ok()),
                },
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Where a pane session's turn stands, if a hook has recorded one.
    pub fn turn_mark(&self, session_id: &str) -> Result<Option<TurnMark>, Error> {
        let mut st = self.conn.prepare(
            "SELECT session_id, ordinal, open, before_commit, before_tree, last_after_tree, user_text, authored_json, tool_calls, started_at
             FROM turn_marks WHERE session_id = ?1",
        )?;
        let mut rows = st.query_map([session_id], |r| {
            Ok(TurnMark {
                session_id: r.get("session_id")?,
                ordinal: r.get("ordinal")?,
                open: r.get::<_, i64>("open")? != 0,
                before_commit: r.get("before_commit")?,
                before_tree: r.get("before_tree")?,
                last_after_tree: r.get("last_after_tree")?,
                user_text: r.get("user_text")?,
                // Only marks files as authored in the turn's changes; an
                // unreadable list loses that hint, not the hook's turn.
                authored: serde_json::from_str(&r.get::<_, String>("authored_json")?).unwrap_or_default(),
                tool_calls: r.get("tool_calls")?,
                started_at: r.get("started_at")?,
            })
        })?;
        rows.next().transpose().map_err(Into::into)
    }

    /// The agent's most recently started session, ended or not.
    pub fn latest_session_for_agent(&self, pubkey: &str) -> Result<Option<Session>, Error> {
        Ok(self
            .list_sessions(SessionFilter {
                agent_pubkey: Some(pubkey.to_string()),
                limit: Some(1),
                ..Default::default()
            })?
            .pop())
    }
}

// ---------------------------------------------------------------------------
// status + usage
// ---------------------------------------------------------------------------

const STATUS_COLS: &str =
    "st.session_id, st.thread_id, st.state, st.kind, st.title, st.msg, st.progress, st.updated_at";

fn row_status(r: &Row<'_>) -> rusqlite::Result<Status> {
    Ok(Status {
        session_id: r.get("session_id")?,
        thread_id: r.get("thread_id")?,
        // Unknown text reads as idle / no kind rather than failing the read.
        state: r.get::<_, String>("state")?.parse().unwrap_or_default(),
        kind: r
            .get::<_, Option<String>>("kind")?
            .and_then(|k| k.parse().ok()),
        title: r.get("title")?,
        msg: r.get("msg")?,
        progress: r
            .get::<_, Option<i64>>("progress")?
            .map(|p| u8::try_from(p.clamp(0, 100)).expect("a percentage clamped to 0..=100 fits in u8")),
        updated_at: r.get("updated_at")?,
    })
}

const USAGE_COLS: &str = "session_id, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, turns, cost_usd_est, updated_at";

fn row_usage(r: &Row<'_>) -> rusqlite::Result<Usage> {
    Ok(Usage {
        session_id: r.get("session_id")?,
        input_tokens: r.get("input_tokens")?,
        output_tokens: r.get("output_tokens")?,
        cache_read_tokens: r.get("cache_read_tokens")?,
        cache_write_tokens: r.get("cache_write_tokens")?,
        turns: r.get("turns")?,
        cost_usd_est: r.get("cost_usd_est")?,
        updated_at: r.get("updated_at")?,
    })
}

fn row_sample(r: &Row<'_>) -> rusqlite::Result<UsageSample> {
    Ok(UsageSample {
        session_id: r.get("session_id")?,
        at: r.get("at")?,
        input_tokens: r.get("input_tokens")?,
        output_tokens: r.get("output_tokens")?,
    })
}

/// A thread's usage: the rows of `effective_usage_log` that name the thread,
/// summed per session and then across its sessions.  Filtering before the
/// `GROUP BY` lets SQLite search both halves of the view by thread; asking
/// `effective_usage` instead builds that whole view on every call.  For a
/// session whose rows all name its own thread, which is what `add_usage`
/// writes, both count the same rows; a session with rows naming several
/// threads is counted here row by row, and there under one of them.
pub(crate) const THREAD_USAGE_SQL: &str = "SELECT '' AS session_id,
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0), COALESCE(SUM(cache_write_tokens), 0),
                    COALESCE(SUM(turns), 0), COALESCE(SUM(cost_usd_est), 0.0), COALESCE(MAX(updated_at), 0)
             FROM (
                 SELECT SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens,
                        SUM(cache_read_tokens) AS cache_read_tokens, SUM(cache_write_tokens) AS cache_write_tokens,
                        SUM(turns) AS turns, SUM(cost_usd_est) AS cost_usd_est, MAX(at) AS updated_at
                 FROM effective_usage_log WHERE thread_id = ?1 GROUP BY session_id
             )";

/// What [`Store::usage_windows`] sums usage by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UsageGrouping {
    Agent,
    Thread,
}

impl UsageGrouping {
    /// The `effective_usage_log` column that names the group.
    fn column(self) -> &'static str {
        match self {
            UsageGrouping::Agent => "agent_pubkey",
            UsageGrouping::Thread => "thread_id",
        }
    }
}

impl Store {
    /// Live status of one session.
    pub fn status(&self, session_id: &str) -> Result<Option<Status>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {STATUS_COLS} FROM status st WHERE st.session_id = ?1"),
                [session_id],
                row_status,
            )
            .optional()?)
    }

    /// Status of every non-ended session, most recently updated first.
    pub fn statuses(&self) -> Result<Vec<Status>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {STATUS_COLS} FROM status st JOIN sessions s ON s.id = st.session_id
             WHERE s.state <> 'ended' ORDER BY st.updated_at DESC, st.session_id"
        ))?;
        let rows = st.query_map([], row_status)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Accumulated usage of one session.
    pub fn usage(&self, session_id: &str) -> Result<Option<Usage>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {USAGE_COLS} FROM effective_usage WHERE session_id = ?1"),
                [session_id],
                row_usage,
            )
            .optional()?)
    }

    /// Usage of every session, most recently updated first.
    pub fn usages(&self) -> Result<Vec<Usage>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {USAGE_COLS} FROM effective_usage ORDER BY updated_at DESC, session_id"
        ))?;
        let rows = st.query_map([], row_usage)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Usage summed over every session of a thread (`session_id = ""`).
    pub fn thread_usage(&self, thread_id: &str) -> Result<Usage, Error> {
        Ok(self.conn.query_row(
            THREAD_USAGE_SQL,
            [thread_id],
            |r| {
                Ok(Usage {
                    session_id: r.get(0)?,
                    input_tokens: r.get(1)?,
                    output_tokens: r.get(2)?,
                    cache_read_tokens: r.get(3)?,
                    cache_write_tokens: r.get(4)?,
                    turns: r.get(5)?,
                    cost_usd_est: r.get(6)?,
                    updated_at: r.get(7)?,
                })
            },
        )?)
    }

    /// What each agent spent, most recent spender first: on `thread` alone,
    /// or (`None`) everywhere.
    pub fn usage_by_agent(&self, now: i64, thread: Option<&str>) -> Result<Vec<UsageWindows>, Error> {
        self.usage_windows(UsageGrouping::Agent, now, None, thread)
    }

    /// What each thread spent, most recent first, at most `limit` of them.
    pub fn usage_by_thread(&self, now: i64, limit: usize) -> Result<Vec<UsageWindows>, Error> {
        self.usage_windows(UsageGrouping::Thread, now, Some(limit), None)
    }

    fn usage_windows(&self, grouping: UsageGrouping, now: i64, limit: Option<usize>, thread: Option<&str>) -> Result<Vec<UsageWindows>, Error> {
        let key = grouping.column();
        // One pass: each window is a sum over the rows inside it.
        let sums = |w: &str| {
            ["input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "reasoning_tokens", "turns", "cost_usd_est"]
                .iter()
                .map(|c| format!("COALESCE(SUM(CASE WHEN {w} THEN {c} END), 0)"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let sql = format!(
            "SELECT {key}, {}, {}, {}, MAX(at) FROM effective_usage_log WHERE ?4 IS NULL OR thread_id = ?4
             GROUP BY {key} ORDER BY MAX(at) DESC, {key} LIMIT ?3",
            sums("at >= ?1"),
            sums("at >= ?2"),
            sums("1"),
        );
        let mut st = self.conn.prepare(&sql)?;
        let limit = limit.map(sql_limit).unwrap_or(-1);
        let rows = st.query_map(params![now - SECONDS_PER_HOUR, now - SECONDS_PER_DAY, limit, thread], |r| {
            let totals = |at: usize| -> rusqlite::Result<UsageTotals> {
                Ok(UsageTotals {
                    input_tokens: r.get(at)?,
                    output_tokens: r.get(at + 1)?,
                    cache_read_tokens: r.get(at + 2)?,
                    cache_write_tokens: r.get(at + 3)?,
                    reasoning_tokens: r.get(at + 4)?,
                    turns: r.get(at + 5)?,
                    cost_usd: r.get(at + 6)?,
                })
            };
            Ok(UsageWindows { key: r.get(0)?, last_hour: totals(1)?, last_day: totals(8)?, total: totals(15)?, last_at: r.get(22)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Tokens spent in each of `buckets` slices of `bucket_secs` ending at
    /// `now`, oldest first: a sparkline's worth.  `agent` `None` is everyone;
    /// `thread` `None` is every thread.
    pub fn usage_series(&self, agent: Option<&str>, thread: Option<&str>, now: i64, bucket_secs: i64, buckets: usize) -> Result<Vec<i64>, Error> {
        let bucket_secs = bucket_secs.max(1);
        let since = now.saturating_sub(bucket_secs.saturating_mul(sql_limit(buckets)));
        let mut out = vec![0i64; buckets];
        let mut statement = self.conn.prepare(
            "SELECT (at - ?1) / ?2, SUM(input_tokens + output_tokens + cache_read_tokens + cache_write_tokens)
             FROM effective_usage_log WHERE at >= ?1 AND at < ?3 AND (?4 IS NULL OR agent_pubkey = ?4)
               AND (?5 IS NULL OR thread_id = ?5) GROUP BY 1",
        )?;
        let rows = statement.query_map(params![since, bucket_secs, now.saturating_add(1), agent, thread], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (bucket, tokens) = row?;
            // `at >= since` makes every bucket index non-negative; the last
            // bucket also takes the second at `now` itself.
            let Ok(bucket) = usize::try_from(bucket) else { continue };
            if let Some(slot) = out.get_mut(bucket.min(buckets.saturating_sub(1))) {
                *slot = slot.saturating_add(tokens);
            }
        }
        Ok(out)
    }

    /// Every plan window a harness reported, by harness then window.
    pub fn plan_limits(&self) -> Result<Vec<PlanLimit>, Error> {
        let mut st = self.conn.prepare("SELECT harness, window, used_pct, resets_at, updated_at FROM plan_limits ORDER BY harness, window")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (harness, window, used_pct, resets_at, updated_at) = row?;
            // A window this build doesn't know is skipped, not an error.
            let Ok(window) = window.parse::<LimitWindow>() else { continue };
            out.push(PlanLimit { harness: Harness::parse(&harness), window, used_pct, resets_at, updated_at });
        }
        out.sort_by_key(|l| (l.harness.to_string(), LimitWindow::ALL.iter().position(|w| *w == l.window)));
        Ok(out)
    }

    /// How full each live session's context window is.
    pub fn session_contexts(&self) -> Result<Vec<SessionContext>, Error> {
        let mut st = self.conn.prepare(
            "SELECT c.session_id, c.used, c.size, c.updated_at FROM session_context c
             JOIN sessions s ON s.id = c.session_id WHERE s.state <> 'ended' ORDER BY c.updated_at DESC",
        )?;
        let rows = st.query_map([], |r| Ok(SessionContext { session_id: r.get(0)?, used: r.get(1)?, size: r.get(2)?, updated_at: r.get(3)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Samples at or after `since` (Unix seconds), oldest first; `session_id`
    /// `None` means every session.
    pub fn usage_samples(
        &self,
        session_id: Option<&str>,
        since: i64,
    ) -> Result<Vec<UsageSample>, Error> {
        let mut st = self.conn.prepare(
            "SELECT session_id, at, input_tokens, output_tokens FROM usage_samples
             WHERE at >= ?1 AND (?2 IS NULL OR session_id = ?2) ORDER BY at ASC, event_id ASC",
        )?;
        let rows = st.query_map(params![since, session_id], row_sample)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Samples of every session of a thread at or after `since`, oldest first.
    pub fn thread_usage_samples(
        &self,
        thread_id: &str,
        since: i64,
    ) -> Result<Vec<UsageSample>, Error> {
        let mut st = self.conn.prepare(
            "SELECT session_id, at, input_tokens, output_tokens FROM usage_samples
             WHERE at >= ?1 AND thread_id = ?2 ORDER BY at ASC, event_id ASC",
        )?;
        let rows = st.query_map(params![since, thread_id], row_sample)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

// ---------------------------------------------------------------------------
// inbox (over projected events)
// ---------------------------------------------------------------------------

fn read_frontier(store: &Store, pubkey: &str) -> Result<Option<(i64, String)>, Error> {
    Ok(store
        .conn
        .query_row(
            "SELECT through_created_at, through_id FROM read_frontiers WHERE pubkey = ?1 AND scope = ?2",
            params![pubkey, INBOX_SCOPE],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// `same_owner(key)`: the owner `?1` and every live device that resolves to
/// it, as many links down as authorization walks (`PRINCIPAL_DEPTH_MAX`).
const SAME_OWNER_KEYS: &str = "WITH RECURSIVE same_owner(key, depth) AS (
        SELECT ?1, 0
        UNION
        SELECT d.pubkey, s.depth + 1 FROM devices d JOIN same_owner s ON d.owner = s.key
        WHERE d.revoked_at IS NULL AND s.depth < 4
    )";

#[cfg(test)]
pub(crate) fn same_owner_sql() -> &'static str {
    SAME_OWNER_KEYS
}

impl Store {
    /// Projected envelopes that mention `pubkey`, newest first.  A message
    /// to any key that speaks for the same owner (a linked device, RFD 0004)
    /// counts: on every machine, the human's inbox is the owner's.
    pub fn inbox(&self, pubkey: &str, limit: usize) -> Result<Vec<Envelope>, Error> {
        let owner = self.principal(pubkey)?;
        let mut st = self.conn.prepare(&format!(
            "{SAME_OWNER_KEYS}
             SELECT {EVENT_COLS} FROM events e
             WHERE e.applied_at IS NOT NULL
               AND e.id IN (SELECT message_id FROM mentions WHERE pubkey IN (SELECT key FROM same_owner))
             ORDER BY e.created_at DESC, e.id DESC LIMIT ?2"
        ))?;
        let rows = st.query_map(params![owner, sql_limit(limit)], row_envelope)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Inbox entries newer than `pubkey`'s read frontier, comparing `(created_at, id)`.
    pub fn unread_count(&self, pubkey: &str) -> Result<u64, Error> {
        let owner = self.principal(pubkey)?;
        let (ts, id) = read_frontier(self, pubkey)?.unwrap_or((i64::MIN, String::new()));
        let n: i64 = self.conn.query_row(
            &format!(
                "{SAME_OWNER_KEYS}
                 SELECT COUNT(*) FROM events e
                 WHERE e.applied_at IS NOT NULL
                   AND e.id IN (SELECT message_id FROM mentions WHERE pubkey IN (SELECT key FROM same_owner))
                   AND (e.created_at > ?2 OR (e.created_at = ?2 AND e.id > ?3))"
            ),
            params![owner, ts, id],
            |r| r.get(0),
        )?;
        Ok(u64::try_from(n).expect("COUNT(*) is never negative"))
    }

    /// Advance `pubkey`'s read frontier; it never moves backwards.
    pub fn mark_read(
        &self,
        pubkey: &str,
        through_created_at: u64,
        through_id: &str,
    ) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO read_frontiers (pubkey, scope, through_created_at, through_id) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (pubkey, scope) DO UPDATE SET
                through_created_at = excluded.through_created_at, through_id = excluded.through_id
             WHERE excluded.through_created_at > read_frontiers.through_created_at
                OR (excluded.through_created_at = read_frontiers.through_created_at
                    AND excluded.through_id > read_frontiers.through_id)",
            params![pubkey, INBOX_SCOPE, sql_integer(through_created_at, "read frontier created_at")?, through_id],
        )?;
        Ok(())
    }

    /// Permission requests (10) and questions (12) without a projected reply
    /// (11/13 carrying `["e", <request id>, ...]`), oldest first.
    pub fn pending_requests(&self) -> Result<Vec<Envelope>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {EVENT_COLS} FROM events e
             WHERE e.kind IN (?1, ?2) AND e.applied_at IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM message_refs r JOIN events reply ON reply.id = r.message_id
                   WHERE r.ref_id = e.id AND reply.kind IN (?3, ?4) AND reply.applied_at IS NOT NULL)
             ORDER BY e.created_at ASC, e.id ASC"
        ))?;
        let rows = st.query_map(
            params![
                kind::PERMISSION_REQUEST,
                kind::QUESTION,
                kind::PERMISSION_REPLY,
                kind::ANSWER
            ],
            row_envelope,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Projected envelopes carrying `["e", id, ...]` (any marker), newest first.
    pub fn replies_to(&self, id: &str) -> Result<Vec<Envelope>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {EVENT_COLS} FROM events e JOIN message_refs r ON r.message_id = e.id
             WHERE r.ref_id = ?1 AND e.applied_at IS NOT NULL ORDER BY e.created_at DESC, e.id DESC"
        ))?;
        let rows = st.query_map([id], row_envelope)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The conversation on a thread (notes, tasks, requests, replies: kinds
    /// below 20), oldest first.
    pub fn thread_messages(&self, thread_id: &str) -> Result<Vec<Envelope>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {EVENT_COLS} FROM events e
             WHERE e.thread_id = ?1 AND e.kind < ?2 AND e.applied_at IS NOT NULL
             ORDER BY e.created_at ASC, e.id ASC"
        ))?;
        let rows = st.query_map(params![thread_id, kind::STATUS], row_envelope)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
