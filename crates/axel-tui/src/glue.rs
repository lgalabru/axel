//! The only module that talks to `axel_store`, `axel_identity` and
//! `axel_inbox`. It converts store records into the pure rows in `model.rs`
//! and performs the few writes the TUI makes (new agent, permission reply,
//! read frontier).

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use axel_identity::{Keypair, Keystore, attest};
use axel_inbox::{Envelope, EnvelopeBuilder, kind};
use axel_store::{AgentColor, NewAgent, Session, SessionFilter, SessionRuntime, SessionState, StatusState, Store, ThreadFilter, ThreadState};
use serde_json::Value;

use crate::model::{
    AgentRow, ChoiceOption, DashAgentRow, DashboardData, InboxData, ItemBody, ItemKind, ItemStatus, LiveSession, MessageRow,
    NewAgentSpec, ProjectRow, RequestKind, RequestRow, StatusRow, ThreadData, ThreadItem, ThreadRow, TokensPanel,
};
use crate::util::bucket_series;

/// Map any crate error into anyhow via Display, so this compiles regardless of
/// whether the other crates' error types are `Send + Sync + 'static`.
fn se<E: std::fmt::Display>(e: E) -> anyhow::Error {
    anyhow!("{e}")
}

pub fn owner_pubkey(keystore: &Keystore) -> Result<String> {
    Ok(keystore.owner().map_err(se).context("loading owner key")?.public().to_string())
}

pub fn load_projects(store: &Store) -> Result<Vec<ProjectRow>> {
    Ok(store
        .recent_projects(20)
        .map_err(se)?
        .into_iter()
        .map(|p| ProjectRow { name: p.name, path: p.path })
        .collect())
}

fn agent_row(a: axel_store::Agent, unread: u64) -> AgentRow {
    AgentRow {
        pubkey: a.pubkey,
        name: a.name,
        harness: a.harness,
        model: a.model,
        color: a.color,
        runtime_mode: a.runtime_mode,
        last_seen_at: a.last_seen_at,
        unread,
    }
}

pub fn load_agents(store: &Store) -> Result<Vec<AgentRow>> {
    let agents = store.list_agents().map_err(se)?;
    Ok(agents
        .into_iter()
        .map(|a| {
            let unread = store.unread_count(&a.pubkey).unwrap_or(0);
            agent_row(a, unread)
        })
        .collect())
}

/// Generate a keypair, store it, attest it with the owner key and insert the agent row.
pub fn create_agent(store: &Store, keystore: &Keystore, spec: &NewAgentSpec) -> Result<AgentRow> {
    if store.agent_by_name(&spec.name).map_err(se)?.is_some() {
        return Err(anyhow!("An agent named \"{}\" already exists.", spec.name));
    }
    let kp = Keypair::generate();
    keystore.put_agent(&kp).map_err(se).context("storing agent key")?;
    let owner = keystore.owner().map_err(se).context("loading owner key")?;
    let tag = attest(&owner, &kp.public(), "").map_err(se).context("attesting agent")?;
    let agent = store
        .insert_agent(&NewAgent {
            pubkey: kp.public().to_string(),
            name: spec.name.clone(),
            harness: spec.harness.clone(),
            model: spec.model.clone(),
            skills: spec.skills.clone(),
            prompt: None,
            color: spec.color,
            runtime_mode: spec.runtime_mode,
            auth_tag: Some(tag.to_tag()),
        })
        .map_err(se)
        .context("inserting agent")?;
    Ok(agent_row(agent, 0))
}

/// pubkey -> (display name, colour) for everyone we know (owner included).
struct Names<'a> {
    store: &'a Store,
    owner_pk: &'a str,
    cache: HashMap<String, (String, Option<AgentColor>)>,
}

impl<'a> Names<'a> {
    fn new(store: &'a Store, owner_pk: &'a str) -> Result<Self> {
        let mut cache = HashMap::new();
        for a in store.list_agents().map_err(se)? {
            cache.insert(a.pubkey, (a.name, a.color));
        }
        Ok(Self { store, owner_pk, cache })
    }

    fn lookup(&mut self, pk: &str) -> (String, Option<AgentColor>) {
        if pk == self.owner_pk {
            return ("you".to_string(), None);
        }
        if let Some(n) = self.cache.get(pk) {
            return n.clone();
        }
        let entry = match self.store.agent_by_pubkey(pk) {
            Ok(Some(a)) => (a.name, a.color),
            _ => (short_pk(pk), None),
        };
        self.cache.insert(pk.to_string(), entry.clone());
        entry
    }

    fn name(&mut self, pk: &str) -> String {
        self.lookup(pk).0
    }
}

/// Dim-able prefix for an unknown pubkey.
fn short_pk(pk: &str) -> String {
    let s: String = pk.chars().take(8).collect();
    format!("{s}…")
}

fn first_tag<'e>(env: &'e Envelope, name: &str) -> Option<&'e str> {
    env.tags.iter().find(|t| t.len() >= 2 && t[0] == name).map(|t| t[1].as_str())
}

fn thread_tag(env: &Envelope) -> Option<String> {
    env.thread_id().or_else(|| first_tag(env, "t")).map(|s| s.to_string())
}

fn session_tag(env: &Envelope) -> Option<String> {
    first_tag(env, "s").map(|s| s.to_string())
}

fn requests(store: &Store, names: &mut Names) -> Result<Vec<RequestRow>> {
    let mut out = Vec::new();
    let mut titles: HashMap<String, Option<String>> = HashMap::new();
    for m in store.pending_requests().map_err(se)? {
        let kind = if m.kind == kind::PERMISSION_REQUEST { RequestKind::Permission } else { RequestKind::Question };
        let thread_id = thread_tag(&m);
        let session_id = session_tag(&m);
        let thread_title = match &thread_id {
            Some(tid) => titles
                .entry(tid.clone())
                .or_insert_with(|| store.get_thread(tid).ok().flatten().map(|t| t.title))
                .clone(),
            None => None,
        };
        out.push(RequestRow {
            agent_name: names.name(&m.from),
            id: m.id,
            from_pubkey: m.from,
            kind,
            content: m.content,
            thread_id,
            session_id,
            thread_title,
            created_at: m.created_at as i64,
        });
    }
    Ok(out)
}

fn messages(store: &Store, owner_pk: &str, names: &mut Names, limit: usize) -> Result<Vec<MessageRow>> {
    Ok(store
        .inbox(owner_pk, limit)
        .map_err(se)?
        .into_iter()
        .map(|m| MessageRow {
            from_name: names.name(&m.from),
            thread_id: thread_tag(&m),
            id: m.id,
            from_pubkey: m.from,
            kind: m.kind,
            kind_label: kind::name(m.kind).to_string(),
            content: m.content,
            created_at: m.created_at,
        })
        .collect())
}

pub fn load_inbox(store: &Store, owner_pk: &str) -> Result<InboxData> {
    let mut names = Names::new(store, owner_pk)?;
    let requests = requests(store, &mut names)?;
    let mut statuses = Vec::new();
    let mut threads: HashMap<String, Option<axel_store::Thread>> = HashMap::new();
    for s in store.statuses().map_err(se)? {
        let session = store.get_session(&s.session_id).ok().flatten();
        let thread = threads
            .entry(s.thread_id.clone())
            .or_insert_with(|| store.get_thread(&s.thread_id).ok().flatten())
            .clone();
        let agent_name = session.as_ref().map(|x| names.name(&x.agent_pubkey)).unwrap_or_else(|| "?".into());
        let thread_title = thread.map(|t| t.title).or_else(|| s.title.clone()).unwrap_or_else(|| s.thread_id.clone());
        statuses.push(StatusRow {
            session_id: s.session_id,
            thread_id: s.thread_id,
            state: s.state,
            agent_name,
            thread_title,
            msg: s.msg,
            updated_at: s.updated_at,
        });
    }
    statuses.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    let messages = messages(store, owner_pk, &mut names, 50)?;
    let unread = store.unread_count(owner_pk).map_err(se)?;
    Ok(InboxData { requests, statuses, messages, unread })
}

/// Sign and post a PERMISSION_REPLY for `req` as the owner.
pub fn reply_permission(store: &Store, keystore: &Keystore, req: &RequestRow, allow: bool) -> Result<()> {
    let owner = keystore.owner().map_err(se).context("loading owner key")?;
    let mut b = EnvelopeBuilder::new(kind::PERMISSION_REPLY, if allow { "allow" } else { "deny" })
        .reply_to(&req.id, &req.id)
        .mention(&req.from_pubkey);
    if let Some(tid) = &req.thread_id {
        b = b.thread(tid);
    }
    if let Some(sid) = &req.session_id {
        b = b.tag(vec!["s".to_string(), sid.clone()]);
    }
    let env = b.sign(&owner).map_err(se).context("signing reply")?;
    store.post(&env).map_err(se).context("posting reply")?;
    Ok(())
}

/// Move the owner's inbox read frontier to the newest message.
pub fn mark_all_read(store: &Store, owner_pk: &str, data: &InboxData) -> Result<bool> {
    let newest = data.messages.iter().max_by_key(|m| (m.created_at, m.id.clone()));
    match newest {
        Some(m) => {
            store.mark_read(owner_pk, m.created_at, &m.id).map_err(se)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

fn samples_to_pairs(samples: Vec<axel_store::UsageSample>) -> Vec<(i64, i64)> {
    samples.into_iter().map(|s| (s.at, s.input_tokens + s.output_tokens)).collect()
}

/// Status state for a session that has no status row yet.
fn session_status_state(s: &Session) -> StatusState {
    match s.state {
        SessionState::Running => StatusState::Working,
        SessionState::Blocked | SessionState::Paused => StatusState::Blocked,
        SessionState::Starting | SessionState::Idle => StatusState::Idle,
        SessionState::Ended => StatusState::Done,
    }
}

/// Live state of a thread: the most urgent status if any, else derived from
/// its live sessions' states (blocked > working > idle), else idle.
fn live_state(status: Option<&axel_store::Status>, sessions_live: &[Session]) -> StatusState {
    if let Some(s) = status {
        return s.state;
    }
    let states: Vec<StatusState> = sessions_live.iter().map(session_status_state).collect();
    for want in [StatusState::Blocked, StatusState::Working] {
        if states.contains(&want) {
            return want;
        }
    }
    StatusState::Idle
}

/// Sort rank for the dashboard tables: blocked/error first, then working, then the rest.
fn sort_urgency(state: StatusState) -> u8 {
    match state {
        StatusState::Blocked | StatusState::Error => 0,
        StatusState::Working => 1,
        StatusState::Idle => 2,
        StatusState::Done => 3,
    }
}

/// Most urgent live session of a thread: blocked first, else most recently started.
fn focus_session(summary: &axel_store::ThreadSummary, statuses: &HashMap<String, axel_store::Status>) -> Option<String> {
    let mut live: Vec<&axel_store::Session> = summary.sessions_live.iter().collect();
    live.sort_by_key(|s| std::cmp::Reverse(s.started_at));
    live.iter()
        .copied()
        .find(|s| statuses.get(&s.id).is_some_and(|st| st.state == StatusState::Blocked))
        .or_else(|| live.first().copied())
        .map(|s| s.id.clone())
}

/// Open threads as dashboard rows, unsorted.
fn thread_rows(
    store: &Store,
    names: &mut Names,
    statuses: &HashMap<String, axel_store::Status>,
    states: Vec<ThreadState>,
    since: i64,
    now: i64,
) -> Result<Vec<ThreadRow>> {
    let summaries = store.thread_summaries(ThreadFilter { states: Some(states), ..Default::default() }).map_err(se)?;
    let mut threads = Vec::with_capacity(summaries.len());
    for s in &summaries {
        let samples = samples_to_pairs(store.thread_usage_samples(&s.thread.id, since).map_err(se)?);
        let agents = s.agents.iter().map(|pk| names.lookup(pk)).collect();
        let projects = s
            .projects
            .iter()
            .map(|p| p.path_hint.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.path_hint.to_string_lossy().into_owned()))
            .collect();
        threads.push(ThreadRow {
            id: s.thread.id.clone(),
            title: s.thread.title.clone(),
            state: live_state(s.live.as_ref(), &s.sessions_live),
            agents,
            projects,
            environments: s.environments.clone(),
            sessions_total: s.sessions_total,
            sessions_live: s.sessions_live.len(),
            tokens: s.usage.input_tokens + s.usage.output_tokens,
            spark: bucket_series(&samples, since, now, 20),
            updated_at: s.thread.updated_at,
            focus_session: focus_session(s, statuses),
            settled: s.thread.state == ThreadState::Done,
        });
    }
    Ok(threads)
}

/// Open then settled threads, most recently active first: the launcher's recent-threads pane.
pub fn load_recent_threads(store: &Store, owner_pk: &str, now: i64) -> Result<Vec<ThreadRow>> {
    let mut names = Names::new(store, owner_pk)?;
    let statuses: HashMap<String, axel_store::Status> =
        store.statuses().map_err(se)?.into_iter().map(|s| (s.session_id.clone(), s)).collect();
    let mut threads = thread_rows(store, &mut names, &statuses, vec![ThreadState::Open, ThreadState::Done], now - 3600, now)?;
    // Open threads first, then the settled shelf; most recent first in each.
    threads.sort_by_key(|t| (t.settled, std::cmp::Reverse(t.updated_at)));
    Ok(threads)
}

pub fn load_dashboard(store: &Store, owner_pk: &str, now: i64) -> Result<DashboardData> {
    let since = now - 3600;
    let mut names = Names::new(store, owner_pk)?;
    let statuses: HashMap<String, axel_store::Status> =
        store.statuses().map_err(se)?.into_iter().map(|s| (s.session_id.clone(), s)).collect();
    let usages: HashMap<String, axel_store::Usage> =
        store.usages().map_err(se)?.into_iter().map(|u| (u.session_id.clone(), u)).collect();

    // ---- threads ----
    let mut threads = thread_rows(store, &mut names, &statuses, vec![ThreadState::Open], since, now)?;
    threads.sort_by_key(|t| (sort_urgency(t.state).min(2), std::cmp::Reverse(t.updated_at)));

    // ---- agents ----
    let mut agents = Vec::new();
    let mut thread_titles: HashMap<String, Option<String>> = HashMap::new();
    for a in store.list_agents().map_err(se)? {
        let latest = store.latest_session_for_agent(&a.pubkey).map_err(se)?;
        let sessions = store
            .list_sessions(SessionFilter { agent_pubkey: Some(a.pubkey.clone()), ..Default::default() })
            .map_err(se)?;
        let tokens: i64 = sessions.iter().filter_map(|s| usages.get(&s.id)).map(|u| u.input_tokens + u.output_tokens).sum();
        let thread_id = latest.as_ref().map(|s| s.thread_id.clone());
        let (thread_title, environment, spark, active, status) = match &latest {
            Some(sess) => {
                let title = thread_titles
                    .entry(sess.thread_id.clone())
                    .or_insert_with(|| store.get_thread(&sess.thread_id).ok().flatten().map(|t| t.title))
                    .clone();
                let samples = samples_to_pairs(store.usage_samples(Some(&sess.id), since).map_err(se)?);
                let live = sess.ended_at.is_none();
                (
                    title,
                    sess.environment_label.clone(),
                    bucket_series(&samples, since, now, 20),
                    if live { Some(sess.id.clone()) } else { None },
                    if live { statuses.get(&sess.id) } else { None },
                )
            }
            None => (None, String::new(), vec![0; 10], None, None),
        };
        let state = match (&status, &active, &latest) {
            (Some(st), _, _) => st.state,
            (None, Some(_), Some(sess)) => session_status_state(sess),
            _ => StatusState::Idle,
        };
        agents.push(DashAgentRow {
            pubkey: a.pubkey,
            name: a.name,
            color: a.color,
            harness: a.harness,
            runtime_mode: a.runtime_mode,
            state,
            thread_id,
            thread_title,
            environment,
            tokens,
            spark,
            last_event_at: status.map(|s| s.updated_at).or(latest.as_ref().map(|s| s.started_at)),
            active_session_id: active,
        });
    }
    agents.sort_by_key(|a| sort_urgency(a.state));

    // ---- tokens ----
    let samples = samples_to_pairs(store.usage_samples(None, since).map_err(se)?);
    let mut tokens = TokensPanel { series: bucket_series(&samples, since, now, 60), ..Default::default() };
    for u in usages.values() {
        tokens.input += u.input_tokens;
        tokens.output += u.output_tokens;
        tokens.cache_read += u.cache_read_tokens;
        tokens.cache_write += u.cache_write_tokens;
        tokens.cost_usd += u.cost_usd_est;
    }

    let requests = requests(store, &mut names)?;
    let messages = messages(store, owner_pk, &mut names, 20)?;
    let unread = store.unread_count(owner_pk).map_err(se)?;
    Ok(DashboardData { threads, agents, tokens, requests, messages, unread })
}

// ---------------------------------------------------------------- thread view

fn map_kind(k: axel_store::ItemKind) -> ItemKind {
    use axel_store::ItemKind as K;
    match k {
        K::UserMessage => ItemKind::UserMessage,
        K::AssistantMessage => ItemKind::AssistantMessage,
        K::Thought => ItemKind::Thought,
        K::ToolCall => ItemKind::ToolCall,
        K::Permission => ItemKind::Permission,
        K::Question => ItemKind::Question,
        K::Answer => ItemKind::Answer,
        K::Error => ItemKind::Error,
        K::TurnEnd => ItemKind::TurnEnd,
    }
}

fn map_status(s: axel_store::ItemStatus) -> ItemStatus {
    use axel_store::ItemStatus as S;
    match s {
        S::Pending => ItemStatus::Pending,
        S::Running => ItemStatus::Running,
        S::Completed => ItemStatus::Completed,
        S::Failed => ItemStatus::Failed,
        S::Cancelled => ItemStatus::Cancelled,
        S::Allowed => ItemStatus::Allowed,
        S::Denied => ItemStatus::Denied,
        S::Answered => ItemStatus::Answered,
    }
}

/// First string found under any of `keys` (top level).
fn str_of<'v>(v: &'v Value, keys: &[&str]) -> Option<&'v str> {
    keys.iter().find_map(|k| v.get(k).and_then(Value::as_str))
}

/// Text of an ACP content block / list of blocks / plain string.
fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks.iter().filter_map(text_of).collect();
            if parts.is_empty() { None } else { Some(parts.join("\n")) }
        }
        Value::Object(_) => str_of(v, &["text"]).map(str::to_string).or_else(|| v.get("content").and_then(text_of)),
        _ => None,
    }
}

fn compact_json(v: &Value, max: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    crate::util::ellipsize(&s.replace('\n', " "), max)
}

fn options_of(v: &Value) -> Vec<ChoiceOption> {
    v.get("options")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|o| {
                    let id = str_of(o, &["optionId", "option_id", "id"])?.to_string();
                    let label = str_of(o, &["name", "label", "text", "title"]).unwrap_or(&id).to_string();
                    Some(ChoiceOption { id, label })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `content.attachments: ["/path", …]` of a user message.
pub(crate) fn attachments_of(content: &Value) -> Vec<std::path::PathBuf> {
    content
        .get("attachments")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|p| p.as_str().or_else(|| str_of(p, &["path"]))).map(std::path::PathBuf::from).collect())
        .unwrap_or_default()
}

/// Decode the store's JSON `content` into an [`ItemBody`] for `kind`.
pub(crate) fn decode_body(kind: ItemKind, content: &Value) -> ItemBody {
    match kind {
        ItemKind::UserMessage | ItemKind::AssistantMessage | ItemKind::Thought => {
            ItemBody::Text(text_of(content).unwrap_or_default())
        }
        ItemKind::Error => ItemBody::Text(str_of(content, &["message", "text", "error"]).map(str::to_string).or_else(|| text_of(content)).unwrap_or_default()),
        ItemKind::ToolCall => {
            let title = str_of(content, &["title", "name", "tool"]).unwrap_or("tool").to_string();
            let kind = str_of(content, &["kind", "toolKind", "tool_kind"]).map(str::to_string);
            let input = ["rawInput", "raw_input", "input", "arguments"]
                .iter()
                .find_map(|k| content.get(k))
                .map(|v| compact_json(v, 200))
                .unwrap_or_default();
            let output: Vec<String> = ["rawOutput", "raw_output", "output", "result", "content"]
                .iter()
                .find_map(|k| content.get(k))
                .and_then(|v| text_of(v).or_else(|| Some(compact_json(v, 4000))))
                .map(|t| t.lines().map(str::to_string).collect())
                .unwrap_or_default();
            let raw = ["rawInput", "raw_input", "input", "arguments"].iter().find_map(|k| content.get(k)).cloned().unwrap_or(Value::Null);
            // A long input is stored cut off, as a string: read what's there.
            let raw = match raw {
                Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::Null),
                v => v,
            };
            let actions = crate::tools::describe_all(&title, kind.as_deref(), &raw);
            let summary = crate::tools::describe_line(&title, kind.as_deref(), &raw);
            ItemBody::Tool { title, kind, input, output, summary, actions }
        }
        ItemKind::Permission | ItemKind::Question => {
            let prompt = str_of(content, &["title", "question", "prompt", "text"])
                .map(str::to_string)
                .or_else(|| content.get("toolCall").and_then(|tc| str_of(tc, &["title"]).map(str::to_string)))
                .or_else(|| content.get("tool_call").and_then(|tc| str_of(tc, &["title"]).map(str::to_string)))
                .unwrap_or_else(|| "permission".to_string());
            let options = options_of(content);
            let chosen = str_of(content, &["chosen", "answer", "selected", "optionId", "option_id"]).map(|c| {
                options.iter().find(|o| o.id == c).map(|o| o.label.clone()).unwrap_or_else(|| c.to_string())
            });
            ItemBody::Choice { prompt, options, chosen }
        }
        ItemKind::Answer => ItemBody::Answered(
            str_of(content, &["label", "name", "optionId", "option_id", "text"]).map(str::to_string).or_else(|| text_of(content)).unwrap_or_default(),
        ),
        ItemKind::TurnEnd => content
            .get("changes")
            .and_then(|c| serde_json::from_value::<axel_store::TurnChanges>(c.clone()).ok())
            .map_or(ItemBody::Empty, |c| changes_body(&c)),
    }
}

/// The change line of a turn, or nothing when it changed no file.
fn changes_body(c: &axel_store::TurnChanges) -> ItemBody {
    if c.files.is_empty() {
        return ItemBody::Empty;
    }
    let (added, removed) = c.totals();
    ItemBody::Changes { files: c.files.len(), authored: c.authored(), added, removed, between_turns: c.between_turns }
}

pub fn load_thread(store: &Store, owner_pk: &str, thread_id: &str, _now: i64) -> Result<ThreadData> {
    let mut names = Names::new(store, owner_pk)?;
    let statuses: HashMap<String, axel_store::Status> =
        store.statuses().map_err(se)?.into_iter().map(|s| (s.session_id.clone(), s)).collect();

    let summary = store.thread_summary(thread_id).map_err(se)?;
    let (title, environments, tokens, cost_usd) = match &summary {
        Some(s) => (s.thread.title.clone(), s.environments.clone(), s.usage.input_tokens + s.usage.output_tokens, s.usage.cost_usd_est),
        None => (thread_id.to_string(), Vec::new(), 0, 0.0),
    };

    let mut sessions = store
        .list_sessions(SessionFilter { thread_id: Some(thread_id.to_string()), ..Default::default() })
        .map_err(se)?;
    sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
    let by_id: HashMap<String, &Session> = sessions.iter().map(|s| (s.id.clone(), s)).collect();

    // Agents on the thread with their live state.
    let mut agents: Vec<(String, Option<axel_store::AgentColor>, axel_store::StatusState)> = Vec::new();
    let agent_pks: Vec<String> = match &summary {
        Some(s) => s.agents.clone(),
        _ => {
            let mut v: Vec<String> = Vec::new();
            for s in &sessions {
                if !v.contains(&s.agent_pubkey) {
                    v.push(s.agent_pubkey.clone());
                }
            }
            v
        }
    };
    for pk in &agent_pks {
        let (name, color) = names.lookup(pk);
        let live: Vec<&Session> = sessions.iter().filter(|s| &s.agent_pubkey == pk && s.ended_at.is_none()).collect();
        let state = live
            .iter()
            .filter_map(|s| if s.state == SessionState::Paused { Some(axel_store::StatusState::Blocked) } else { statuses.get(&s.id).map(|st| st.state) })
            .min_by_key(|st| status_urgency(*st))
            .unwrap_or_else(|| live.first().map(|s| session_live_state(s)).unwrap_or(axel_store::StatusState::Idle));
        agents.push((name, color, state));
    }

    let local_env = store.local_environment().map(|e| e.id).unwrap_or_default();
    let live_sessions: Vec<LiveSession> = sessions
        .iter()
        .filter(|s| s.ended_at.is_none() && agent_pks.contains(&s.agent_pubkey))
        .map(|s| LiveSession {
            id: s.id.clone(),
            agent_name: names.name(&s.agent_pubkey),
            managed: s.runtime == SessionRuntime::Managed,
            started_at: s.started_at,
            mode: s.runtime_mode,
            host: (s.environment_id != local_env).then(|| {
                store.environment(&s.environment_id).ok().flatten().map_or_else(|| s.environment_label.clone(), |e| e.label)
            }),
            last_reply_at: None,
        })
        .collect();
    let mut live_sessions = live_sessions;
    let last_agent_name = sessions.iter().find(|s| agent_pks.contains(&s.agent_pubkey)).map(|s| names.name(&s.agent_pubkey));

    // Local items.
    let mut items: Vec<ThreadItem> = Vec::new();
    let mut sessions_with_items: std::collections::HashSet<String> = std::collections::HashSet::new();
    for it in store.thread_items(thread_id, 500).map_err(se)? {
        sessions_with_items.insert(it.session_id.clone());
        let kind = map_kind(it.kind);
        let from = it.content.get("from").and_then(|v| v.as_str());
        let agent = if let (ItemKind::UserMessage, Some(pk)) = (kind, from) {
            // Relayed from another agent (`axel send`): show it under that agent's badge.
            Some(names.lookup(pk))
        } else if kind == ItemKind::UserMessage || kind == ItemKind::Answer {
            None
        } else {
            by_id.get(&it.session_id).map(|s| names.lookup(&s.agent_pubkey))
        };
        // A relayed message sits in the recipient's session.
        let relay_to = from.and(by_id.get(&it.session_id)).map(|s| names.name(&s.agent_pubkey));
        items.push(ThreadItem {
            id: it.id,
            session_id: it.session_id,
            agent,
            author: None,
            seq: it.seq,
            turn: it.turn,
            at_ms: it.at,
            kind,
            status: it.status.map(map_status),
            body: decode_body(kind, &it.content),
            attachments: attachments_of(&it.content),
            relay_to,
            steering: it.content.get("steering").and_then(|v| v.as_bool()).unwrap_or(false),
        });
    }

    // The visible window is bounded. Falling outside it does not make a local
    // session remote: otherwise its summary gains a new row identity and the
    // append-only stream prints old conversation beneath today's messages.
    let mut has_local_items: std::collections::HashMap<String, bool> = sessions_with_items.into_iter().map(|id| (id, true)).collect();
    // Turn summaries stand in only when the session has no local items at all.
    for t in store.turns(thread_id).map_err(se)? {
        let local = match has_local_items.get(&t.session_id) {
            Some(found) => *found,
            None => {
                let found = !store.items(&t.session_id, 0, 1).map_err(se)?.is_empty();
                has_local_items.insert(t.session_id.clone(), found);
                found
            }
        };
        if local {
            continue;
        }
        let agent = by_id.get(&t.session_id).map(|s| names.lookup(&s.agent_pubkey)).or_else(|| Some(names.lookup(&t.from_pubkey)));
        let at_ms = t.created_at as i64 * 1000;
        let author = if t.from_pubkey == owner_pk { None } else { store.known_identity_name(&t.from_pubkey).ok().flatten() };
        items.push(ThreadItem {
            id: format!("{}:u", t.id),
            session_id: t.session_id.clone(),
            agent: None,
            author,
            seq: t.summary.ordinal * 3,
            turn: t.summary.ordinal,
            at_ms,
            kind: ItemKind::UserMessage,
            status: Some(ItemStatus::Completed),
            body: ItemBody::Text(t.summary.user_text.clone()),
            attachments: Vec::new(),
            relay_to: None,
            steering: false,
        });
        let mut text = t.summary.assistant_text.clone();
        if t.summary.tool_calls > 0 {
            text.push_str(&format!(
                "\n· {} tool call{} · {} file{} edited · {:.1}s · {}",
                t.summary.tool_calls,
                if t.summary.tool_calls == 1 { "" } else { "s" },
                t.summary.files_edited,
                if t.summary.files_edited == 1 { "" } else { "s" },
                t.summary.duration_ms as f64 / 1000.0,
                t.summary.stop_reason
            ));
        }
        let changes = t.summary.changes.as_ref().map(changes_body).filter(|b| *b != ItemBody::Empty);
        items.push(ThreadItem {
            id: format!("{}:a", t.id),
            session_id: t.session_id.clone(),
            agent: agent.clone(),
            author: None,
            seq: t.summary.ordinal * 3 + 1,
            turn: t.summary.ordinal,
            at_ms: at_ms + 1,
            kind: ItemKind::AssistantMessage,
            status: Some(ItemStatus::Completed),
            body: ItemBody::Text(text),
            attachments: Vec::new(),
            relay_to: None,
            steering: false,
        });
        if let Some(body) = changes {
            items.push(ThreadItem {
                id: format!("{}:c", t.id),
                session_id: t.session_id,
                agent,
                author: None,
                seq: t.summary.ordinal * 3 + 2,
                turn: t.summary.ordinal,
                at_ms: at_ms + 2,
                kind: ItemKind::TurnEnd,
                status: Some(ItemStatus::Completed),
                body,
                attachments: Vec::new(),
                relay_to: None,
                steering: false,
            });
        }
    }
    items.sort_by_key(|i| (i.at_ms, i.seq));
    // Who spoke last decides where a message without a mention goes.
    for live in &mut live_sessions {
        live.last_reply_at = items.iter().filter(|i| i.session_id == live.id && i.kind == ItemKind::AssistantMessage).map(|i| i.at_ms).max();
    }

    let known_agents = store.list_agents().map_err(se)?.into_iter().map(|a| (a.name, a.color)).collect();
    Ok(ThreadData { thread_id: thread_id.to_string(), title, agents, environments, tokens, cost_usd, items, live_sessions, last_agent_name, known_agents })
}

fn status_urgency(s: axel_store::StatusState) -> u8 {
    use axel_store::StatusState as S;
    match s {
        S::Blocked => 0,
        S::Error => 1,
        S::Working => 2,
        S::Done => 3,
        S::Idle => 4,
    }
}

fn session_live_state(s: &Session) -> axel_store::StatusState {
    use axel_store::StatusState as S;
    match s.state {
        SessionState::Running => S::Working,
        SessionState::Blocked | SessionState::Paused => S::Blocked,
        SessionState::Ended => S::Done,
        SessionState::Starting | SessionState::Idle => S::Idle,
    }
}

/// Everything the usage pane shows.
pub fn load_usage(store: &Store, now: i64) -> Result<crate::model::UsageData> {
    use crate::model::{AgentSpend, ThreadSpend, UsageData};
    let agents: HashMap<String, axel_store::Agent> = store.list_agents().map_err(se)?.into_iter().map(|a| (a.pubkey.clone(), a)).collect();
    // The fullest live context of each agent.
    let mut context: HashMap<String, (i64, i64)> = HashMap::new();
    for c in store.session_contexts().map_err(se)? {
        let Some(s) = store.get_session(&c.session_id).map_err(se)? else { continue };
        let fill = |u: i64, n: i64| u as f64 / n.max(1) as f64;
        let e = context.entry(s.agent_pubkey).or_insert((c.used, c.size));
        if fill(c.used, c.size) > fill(e.0, e.1) {
            *e = (c.used, c.size);
        }
    }
    let mut data = UsageData::default();
    for w in store.usage_by_agent(now).map_err(se)? {
        let agent = agents.get(&w.key);
        add(&mut data.last_hour, &w.last_hour);
        add(&mut data.last_day, &w.last_day);
        add(&mut data.total, &w.total);
        data.agents.push(AgentSpend {
            name: agent.map(|a| a.name.clone()).unwrap_or_else(|| w.key.chars().take(8).collect()),
            color: agent.and_then(|a| a.color),
            // A turn still running has spent without finishing a turn yet.
            spark: if w.last_hour.tokens() > 0 { store.usage_series(Some(&w.key), now, 60, 60).map_err(se)? } else { vec![0; 60] },
            context: context.get(&w.key).copied(),
            last_hour: w.last_hour,
            last_day: w.last_day,
            total: w.total,
        });
    }
    for w in store.usage_by_thread(now, 20).map_err(se)? {
        if w.last_day.turns == 0 {
            break; // most recent first: the rest are older still
        }
        let title = store.get_thread(&w.key).map_err(se)?.map(|t| t.title).unwrap_or_else(|| w.key.chars().take(8).collect());
        data.threads.push(ThreadSpend { title, last_day: w.last_day, total: w.total });
    }
    for l in store.plan_limits().map_err(se)? {
        let harness = l.harness.to_string();
        let row = crate::model::LimitRow {
            window: l.window,
            used_pct: if l.resets_at.is_some_and(|r| r <= now) { None } else { Some(l.used_pct) },
            resets_in: l.resets_at.map(|r| r - now).filter(|s| *s > 0),
        };
        match data.limits.last_mut() {
            Some(p) if p.harness == harness => p.windows.push(row),
            _ => data.limits.push(crate::model::PlanRow { harness, windows: vec![row] }),
        }
    }
    Ok(data)
}

fn add(to: &mut axel_store::UsageTotals, u: &axel_store::UsageTotals) {
    to.input_tokens += u.input_tokens;
    to.output_tokens += u.output_tokens;
    to.cache_read_tokens += u.cache_read_tokens;
    to.cache_write_tokens += u.cache_write_tokens;
    to.reasoning_tokens += u.reasoning_tokens;
    to.turns += u.turns;
    to.cost_usd += u.cost_usd;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_local_session_falling_outside_the_window_does_not_reappear_as_a_summary() {
        use axel_store::{Harness, Keypair, NewItem, NewSession, RuntimeMode, SessionRuntime, TurnSummary};
        let store = Store::open_memory().unwrap();
        let owner = Keypair::generate();
        let pk = owner.public().to_base58();
        let thread = store.create_thread(&owner, "Long thread", "Long thread", None).unwrap();
        let env = store.local_environment().unwrap();
        let start = || store.start_session(&owner, &NewSession {
            thread_id: thread.id.clone(), agent_pubkey: pk.clone(),
            environment_id: env.id.clone(), environment_label: env.label.clone(),
            project_repo_key: None, harness: Harness::Codex, runtime_mode: RuntimeMode::default(),
            resumed_from: None, runtime: SessionRuntime::Managed,
        }).unwrap();
        let old = start();
        let current = start();
        let remote = start();
        let item = |session: &str, text: &str| store.push_item(&NewItem {
            session_id: session.into(), thread_id: thread.id.clone(), turn: 1,
            kind: axel_store::ItemKind::AssistantMessage, status: Some(axel_store::ItemStatus::Completed),
            ref_id: None, content: json!({"text": text}),
        }).unwrap();
        item(&old.id, "Old introduction");
        store.record_turn(&owner, &old.id, &TurnSummary { ordinal: 1, user_text: "Introduce yourself".into(), assistant_text: "Old introduction".into(), ..Default::default() }).unwrap();
        store.record_turn(&owner, &remote.id, &TurnSummary { ordinal: 1, user_text: "Remote prompt".into(), assistant_text: "Remote reply".into(), ..Default::default() }).unwrap();
        for _ in 0..499 { item(&current.id, "Current work"); }
        let before = load_thread(&store, &pk, &thread.id, 0).unwrap();
        assert!(before.items.iter().any(|i| i.session_id == old.id));
        item(&current.id, "Cross the window boundary");
        let after = load_thread(&store, &pk, &thread.id, 0).unwrap();
        assert!(!after.items.iter().any(|i| i.session_id == old.id), "old local history must not acquire new summary rows");
        assert!(after.items.iter().any(|i| i.session_id == remote.id && i.body == ItemBody::Text("Remote reply".into())), "sessions with no local items still need their shared turn summaries");
    }

    #[test]
    fn decode_tool_call_permission_and_text() {
        let tool = decode_body(
            ItemKind::ToolCall,
            &json!({"title": "Read src/main.rs", "kind": "read", "rawInput": {"path": "src/main.rs"}, "content": [{"type":"content","content":{"type":"text","text":"fn main() {}\nok"}}]}),
        );
        assert_eq!(
            tool,
            ItemBody::Tool {
                title: "Read src/main.rs".into(),
                kind: Some("read".into()),
                input: "{\"path\":\"src/main.rs\"}".into(),
                output: vec!["fn main() {}".into(), "ok".into()],
                summary: "Read main.rs".into(),
                actions: vec![crate::tools::Action { verb: crate::tools::Verb::Read, object: Some("main.rs".into()), text: "Read main.rs".into() }],
            }
        );
        let perm = decode_body(
            ItemKind::Permission,
            &json!({"toolCall": {"title": "Run cargo test"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}, {"optionId": "reject", "name": "Reject"}], "chosen": "allow"}),
        );
        assert_eq!(
            perm,
            ItemBody::Choice {
                prompt: "Run cargo test".into(),
                options: vec![ChoiceOption { id: "allow".into(), label: "Allow".into() }, ChoiceOption { id: "reject".into(), label: "Reject".into() }],
                chosen: Some("Allow".into()),
            }
        );
        assert_eq!(decode_body(ItemKind::AssistantMessage, &json!({"text": "hi"})), ItemBody::Text("hi".into()));
        assert_eq!(decode_body(ItemKind::UserMessage, &json!("plain")), ItemBody::Text("plain".into()));
        assert_eq!(decode_body(ItemKind::Error, &json!({"message": "boom"})), ItemBody::Text("boom".into()));
        assert_eq!(decode_body(ItemKind::Answer, &json!({"optionId": "allow"})), ItemBody::Answered("allow".into()));
        assert_eq!(decode_body(ItemKind::TurnEnd, &json!({})), ItemBody::Empty);
        let changes = json!({ "before": "a", "after": "b", "between_turns": true, "files": [
            { "path": "src/a.rs", "status": "modified", "added": 4, "removed": 1, "authored": true },
            { "path": "Cargo.lock", "status": "modified", "added": 2, "removed": 2 },
            { "path": "img.png", "status": "added", "added": null, "removed": null, "authored": true },
        ]});
        assert_eq!(
            decode_body(ItemKind::TurnEnd, &json!({ "stop_reason": "end_turn", "changes": changes })),
            ItemBody::Changes { files: 3, authored: 2, added: 6, removed: 3, between_turns: true }
        );
        assert_eq!(
            decode_body(ItemKind::TurnEnd, &json!({ "changes": { "before": "a", "after": "a", "files": [] } })),
            ItemBody::Empty,
            "a turn that changed nothing has no change line"
        );
        assert_eq!(
            attachments_of(&json!({"text": "see", "attachments": ["/tmp/a.png", {"path": "/tmp/b.txt"}]})),
            vec![std::path::PathBuf::from("/tmp/a.png"), std::path::PathBuf::from("/tmp/b.txt")]
        );
        assert!(attachments_of(&json!({"text": "none"})).is_empty());
    }
}

