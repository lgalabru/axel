//! Reads over the projections and the inbox.

use std::path::PathBuf;

use axel_inbox::Envelope;
use rusqlite::{OptionalExtension, Row, ToSql, params, params_from_iter};

use crate::log::{EVENT_COLS, row_envelope};
use crate::wire::kind;
use crate::{TurnMark, 
    Error, Harness, INBOX_SCOPE, Participant, Session, SessionFilter, Status, Store, Thread,
    ThreadFilter, ThreadProject, ThreadSummary, ThreadWorkspace, Turn, TurnSummary, Usage,
    UsageSample, UsageTotals, UsageWindows, SessionContext, LimitWindow, PlanLimit,
};

fn conv<E: std::error::Error + Send + Sync + 'static>(e: E) -> rusqlite::Error {
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
        state: r.get::<_, String>("state")?.parse().map_err(conv)?,
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
            let mut ph = Vec::new();
            for s in states {
                args.push(Box::new(s.as_str()));
                ph.push(format!("?{}", args.len()));
            }
            sql.push_str(&format!(" AND t.state IN ({})", ph.join(", ")));
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
            args.push(Box::new(limit as i64));
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
        let agents = self.distinct(
            "SELECT DISTINCT agent_pubkey FROM sessions WHERE thread_id = ?1 ORDER BY 1",
            &id,
        )?;
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
                        .map_err(conv)?,
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
            for tp in &mut out {
                tp.local_project_id = match &tp.repo_key {
                    Some(k) => self.project_by_repo_key(&env, k)?.map(|p| p.id),
                    None => None,
                }
                .or(self
                    .conn
                    .query_row(
                        "SELECT id FROM projects WHERE environment_id = ?1 AND path = ?2",
                        params![env, tp.path_hint.to_string_lossy()],
                        |r| r.get(0),
                    )
                    .optional()?);
            }
        }
        Ok(out)
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
                role: r.get::<_, String>("role")?.parse().map_err(conv)?,
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
        state: r.get::<_, String>("state")?.parse().map_err(conv)?,
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
            let mut ph = Vec::new();
            for s in states {
                args.push(Box::new(s.as_str()));
                ph.push(format!("?{}", args.len()));
            }
            sql.push_str(&format!(" AND s.state IN ({})", ph.join(", ")));
        }
        sql.push_str(" ORDER BY s.started_at DESC, s.id DESC");
        if let Some(limit) = f.limit {
            args.push(Box::new(limit as i64));
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
                created_at: r.get::<_, i64>("created_at")? as u64,
                summary: TurnSummary {
                    ordinal: r.get("ordinal")?,
                    user_text: r.get("user_text")?,
                    assistant_text: r.get("assistant_text")?,
                    tool_calls: r.get("tool_calls")?,
                    files_edited: r.get("files_edited")?,
                    duration_ms: r.get("duration_ms")?,
                    stop_reason: r.get("stop_reason")?,
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
            .map(|p| p.clamp(0, 100) as u8),
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
                &format!("SELECT {USAGE_COLS} FROM usage WHERE session_id = ?1"),
                [session_id],
                row_usage,
            )
            .optional()?)
    }

    /// Usage of every session, most recently updated first.
    pub fn usages(&self) -> Result<Vec<Usage>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {USAGE_COLS} FROM usage ORDER BY updated_at DESC, session_id"
        ))?;
        let rows = st.query_map([], row_usage)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Usage summed over every session of a thread (`session_id = ""`).
    pub fn thread_usage(&self, thread_id: &str) -> Result<Usage, Error> {
        Ok(self.conn.query_row(
            "SELECT '' AS session_id,
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0), COALESCE(SUM(cache_write_tokens), 0),
                    COALESCE(SUM(turns), 0), COALESCE(SUM(cost_usd_est), 0.0), COALESCE(MAX(updated_at), 0)
             FROM usage WHERE thread_id = ?1",
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

    /// What each agent spent, most recent spender first.
    pub fn usage_by_agent(&self, now: i64) -> Result<Vec<UsageWindows>, Error> {
        self.usage_windows("agent_pubkey", now, None)
    }

    /// What each thread spent, most recent first, at most `limit` of them.
    pub fn usage_by_thread(&self, now: i64, limit: usize) -> Result<Vec<UsageWindows>, Error> {
        self.usage_windows("thread_id", now, Some(limit))
    }

    fn usage_windows(&self, key: &str, now: i64, limit: Option<usize>) -> Result<Vec<UsageWindows>, Error> {
        // One pass: each window is a sum over the rows inside it.
        let sums = |w: &str| {
            ["input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "reasoning_tokens", "turns", "cost_usd_est"]
                .iter()
                .map(|c| format!("COALESCE(SUM(CASE WHEN {w} THEN {c} END), 0)"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let sql = format!(
            "SELECT {key}, {}, {}, {}, MAX(at) FROM usage_log GROUP BY {key} ORDER BY MAX(at) DESC, {key} LIMIT ?3",
            sums("at >= ?1"),
            sums("at >= ?2"),
            sums("1"),
        );
        let mut st = self.conn.prepare(&sql)?;
        let limit = limit.map(|l| l as i64).unwrap_or(-1);
        let rows = st.query_map(params![now - 3600, now - 86_400, limit], |r| {
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
    /// `now`, oldest first: a sparkline's worth.  `agent` `None` is everyone.
    pub fn usage_series(&self, agent: Option<&str>, now: i64, bucket_secs: i64, buckets: usize) -> Result<Vec<i64>, Error> {
        let bucket_secs = bucket_secs.max(1);
        let since = now - bucket_secs * buckets as i64;
        let mut out = vec![0i64; buckets];
        let mut st = self.conn.prepare(
            "SELECT (at - ?1) / ?2, SUM(input_tokens + output_tokens + cache_read_tokens + cache_write_tokens)
             FROM usage_log WHERE at >= ?1 AND at < ?3 AND (?4 IS NULL OR agent_pubkey = ?4) GROUP BY 1",
        )?;
        let rows = st.query_map(params![since, bucket_secs, now + 1, agent], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (i, n) = row?;
            if let Some(slot) = usize::try_from(i).ok().and_then(|i| out.get_mut(i.min(buckets.saturating_sub(1)))) {
                *slot += n;
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

impl Store {
    /// Projected envelopes that mention `pubkey`, newest first.
    pub fn inbox(&self, pubkey: &str, limit: usize) -> Result<Vec<Envelope>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {EVENT_COLS} FROM events e JOIN mentions m ON m.message_id = e.id
             WHERE m.pubkey = ?1 AND e.applied_at IS NOT NULL
             ORDER BY e.created_at DESC, e.id DESC LIMIT ?2"
        ))?;
        let rows = st.query_map(params![pubkey, limit as i64], row_envelope)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Inbox entries newer than `pubkey`'s read frontier, comparing `(created_at, id)`.
    pub fn unread_count(&self, pubkey: &str) -> Result<u64, Error> {
        let n: i64 = match read_frontier(self, pubkey)? {
            None => self.conn.query_row(
                "SELECT COUNT(*) FROM mentions m JOIN events e ON e.id = m.message_id
                 WHERE m.pubkey = ?1 AND e.applied_at IS NOT NULL",
                [pubkey],
                |r| r.get(0),
            )?,
            Some((ts, id)) => self.conn.query_row(
                "SELECT COUNT(*) FROM mentions m JOIN events e ON e.id = m.message_id
                 WHERE m.pubkey = ?1 AND e.applied_at IS NOT NULL
                   AND (e.created_at > ?2 OR (e.created_at = ?2 AND e.id > ?3))",
                params![pubkey, ts, id],
                |r| r.get(0),
            )?,
        };
        Ok(n as u64)
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
            params![pubkey, INBOX_SCOPE, through_created_at as i64, through_id],
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
