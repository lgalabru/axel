//! `axel expedite "…"`: the quickest way through Axel, for when there is
//! little time or little network.  One agent, a fresh context, no launcher,
//! window or panes: a managed session starts out of sight and its reply
//! streams here.  When the turn ends the session stays up, idle, for a while
//! (`--keep`, 10 minutes by default) and then ends itself, its conversation
//! kept: `-c` within that time sends to the live session, skipping the cold
//! start; after it, `-c` resumes the conversation in a new session.
//! `axel expedite --warm` starts a spare session ahead of time, which the next
//! `axel expedite "…"` in the same directory, with the same agent, takes.
//! `axel thread open` shows it all.
//!
//! The session is an ordinary one (same host, store and policy), so nothing
//! here drives the harness: this only reads what the host records and, when
//! the agent asks something, answers it through the host's socket.  No
//! process outlives its use: a session kept warm ends itself once idle.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axel_acp::socket::{Command, Reply};
use axel_i18n::t;
use axel_store::{ConversationItem, ItemKind, ItemStatus, SessionRuntime, SessionState};
use axel_tui::model::ItemBody;
use axel_tui::{SendCommand, SendReply};
use serde::{Deserialize, Serialize};

use super::Ctx;

/// How often the store is read for what the session wrote.
const POLL_MS: u64 = 50;
/// Most items read in one poll.
const ITEMS_PER_POLL_MAX: usize = 500;
/// Pages of [`ITEMS_PER_POLL_MAX`] read to find a session's last item: half
/// a million items, far more than any one session records.
const ITEM_PAGES_MAX: usize = 1000;
/// A turn longer than this is left to run; expedite stops following it.
const TURN_WAIT_MAX_SECS: u64 = 60 * 60;
/// How long to wait for the session to record its first item.
const START_WAIT_MAX_SECS: u64 = 120;
/// How long a warm session has to record the prompt it took.  It does so
/// before any repository I/O, so only a host that broke between answering
/// and recording waits this out.
const PROMPT_RECORD_WAIT_MAX_SECS: u64 = 10;
/// Longest a thread's title is, cut from the prompt.
const TITLE_CHARS_MAX: usize = 60;
/// Where the last thread and session expedite used are kept, for `-c`.
const LAST_FILE: &str = "expedite-last";
/// Where the spare session `--warm` started is kept, for the next expedite.
const SPARE_FILE: &str = "expedite-spare";
/// Largest of the notes above read back: they hold a few ids and a path.
const STATE_FILE_BYTES_MAX: u64 = 16 * 1024;
/// The reason the session ends with: its conversation resumes on the next message.
const DONE_REASON: &str = "expedite: turn done";
/// The reason a spare ends with when `--warm` starts another in its place.
const SPARE_REPLACED_REASON: &str = "expedite: spare replaced";
/// Largest answer line read from the terminal.
const ANSWER_BYTES_MAX: usize = 1024;
/// How long a session stays up idle after its turn, by default: long enough
/// to read the reply and ask the next thing, short enough that a forgotten
/// one doesn't sit for an hour.
const KEEP_DEFAULT: Duration = Duration::from_secs(10 * 60);
/// How long a spare waits for its prompt, by default: it was started for a
/// prompt that is coming soon.
const SPARE_KEEP_DEFAULT: Duration = Duration::from_secs(30 * 60);

/// Ctrl+C presses: the first cancels the turn, the second leaves at once.
static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_interrupt(_: libc::c_int) {
    INTERRUPTS.fetch_add(1, Ordering::SeqCst);
}

/// What `axel expedite` was asked.
pub struct Request {
    pub prompt: String,
    pub agent: Option<String>,
    /// Continue the last thread expedite used.
    pub resume_last: bool,
    pub thread: Option<String>,
    /// How long a session started now stays up idle once its turn is done;
    /// `None` takes the default.  Zero stops it at once.
    pub keep: Option<Duration>,
    /// Start a spare session for the next expedite instead of a turn.
    pub warm: bool,
}

pub fn run(request: Request) -> Result<()> {
    if request.warm {
        return warm(request);
    }
    let prompt = request.prompt.trim().to_string();
    if prompt.is_empty() {
        bail!("{}", t!("what should the agent do? `axel expedite \"fix the failing test\"`"));
    }
    let keep = request.keep.unwrap_or(KEEP_DEFAULT);
    keep_for_hosts(keep);
    let mut ctx = Ctx::open()?;
    // No window, no panes: without Rex the session host runs detached.
    ctx.rex = None;
    let agent = pick_agent(&ctx, request.agent.as_deref())?;
    let started = Instant::now();
    let turn = match take_warm(&ctx, &request, &agent, &prompt)? {
        Some(turn) => turn,
        None => start_cold(&ctx, &request, &agent, &prompt, keep)?,
    };
    remember_last(&Last { thread_id: turn.thread_id.clone(), session_id: Some(turn.session_id.clone()), keep_secs: Some(turn.keep.as_secs()) });
    // SAFETY: the handler only bumps an atomic counter.
    unsafe { libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t) };
    let outcome = follow(&ctx, &turn.session_id, turn.from_seq);
    let elapsed = started.elapsed();
    // Kept warm, the session ends itself once idle for `keep`; one left
    // running does too, once its turn is done.
    let is_stopped_now = match &outcome {
        Ok(Outcome::Left) => false,
        Ok(_) => turn.keep.is_zero(),
        Err(_) => true,
    };
    if is_stopped_now {
        stop(&ctx, &turn.session_id);
    }
    let outcome = outcome?;
    let kept = if is_stopped_now { Duration::ZERO } else { turn.keep };
    summary(&ctx, &turn, elapsed, outcome, kept);
    Ok(())
}

/// The turn expedite follows: on which session, from which item, and how
/// long that session stays up idle (fixed when it started).
struct Turn {
    thread_id: String,
    session_id: String,
    /// Items after this seq are the turn's.
    from_seq: i64,
    keep: Duration,
}

/// Every session host started from here ends itself after `keep` idle.
fn keep_for_hosts(keep: Duration) {
    assert!(keep <= axel_acp::IDLE_EXIT_AFTER_MAX, "--keep is bounded when it is parsed");
    // SAFETY: called first thing, before anything here starts a thread; the
    // hosts started later inherit it.
    unsafe { std::env::set_var(super::session::IDLE_EXIT_ENV, keep.as_secs().to_string()) };
}

/// A session already running, idle, for this turn: the last expedite's for
/// `-c`, or a spare for a new one.  `None` when there is none to take.
fn take_warm(ctx: &Ctx, request: &Request, agent: &axel_store::Agent, prompt: &str) -> Result<Option<Turn>> {
    if request.thread.is_some() {
        return Ok(None);
    }
    if request.resume_last {
        return take_last(ctx, agent, prompt);
    }
    take_spare(ctx, agent, prompt)
}

/// `-c`: the last expedite's session, when it is still warm.
fn take_last(ctx: &Ctx, agent: &axel_store::Agent, prompt: &str) -> Result<Option<Turn>> {
    let Some(last) = read_last() else { return Ok(None) };
    let Some(session_id) = last.session_id else { return Ok(None) };
    let Some(session) = warm_session(ctx, &session_id, &agent.pubkey)? else { return Ok(None) };
    assert_eq!(session.thread_id, last.thread_id, "the last session is on the last thread");
    let Some(from_seq) = send_prompt(ctx, &session, prompt)? else { return Ok(None) };
    let keep = last.keep_secs.map_or(KEEP_DEFAULT, Duration::from_secs);
    Ok(Some(Turn { thread_id: session.thread_id, session_id, from_seq, keep }))
}

/// A new expedite: the spare `--warm` started, when it is here, for this
/// agent, and still warm.  It is used once.
fn take_spare(ctx: &Ctx, agent: &axel_store::Agent, prompt: &str) -> Result<Option<Turn>> {
    let Some(spare) = read_spare() else { return Ok(None) };
    if spare.cwd != current_dir()? {
        return Ok(None);
    }
    if spare.agent_pubkey != agent.pubkey {
        return Ok(None);
    }
    let Some(session) = warm_session(ctx, &spare.session_id, &agent.pubkey)? else { return Ok(None) };
    // Claimed by moving its note away: of two expedites at once, one wins.
    let claimed = crate::paths::home().join(format!("{SPARE_FILE}.{}", std::process::id()));
    if std::fs::rename(crate::paths::home().join(SPARE_FILE), &claimed).is_err() {
        return Ok(None);
    }
    // The claim is made; a note left behind is never read again.
    let _ = std::fs::remove_file(&claimed);
    let Some(from_seq) = send_prompt(ctx, &session, prompt)? else { return Ok(None) };
    ctx.store.update_thread(&ctx.owner()?, &session.thread_id, Some(&title_of(prompt)), None)?;
    Ok(Some(Turn { thread_id: session.thread_id, session_id: spare.session_id, from_seq, keep: Duration::from_secs(spare.keep_secs) }))
}

/// A new session for this turn: on the thread asked for, the last one, or a
/// new one titled after the prompt.
fn start_cold(ctx: &Ctx, request: &Request, agent: &axel_store::Agent, prompt: &str, keep: Duration) -> Result<Turn> {
    let thread_id = match (&request.thread, request.resume_last) {
        (Some(t), _) => super::ui::resolve_thread_id(ctx, t)?,
        (None, true) => last_thread(ctx)?,
        (None, false) => super::thread::record_thread(ctx, &title_of(prompt), &[current_dir()?])?.id,
    };
    // `@local`: a remote host is a network hop further, which is what this avoids.
    let session_id = super::ui::summon_with(ctx, &thread_id, &format!("{}@local", agent.name), prompt, &[], false)?;
    Ok(Turn { thread_id, session_id, from_seq: 0, keep })
}

/// The session, when it is live, here, this agent's, and idle with nothing
/// waiting, as its host says: one a prompt starts a turn in at once.
fn warm_session(ctx: &Ctx, session_id: &str, agent_pubkey: &str) -> Result<Option<axel_store::Session>> {
    let Some(session) = ctx.store.get_session(session_id)? else { return Ok(None) };
    if session.ended_at.is_some() {
        return Ok(None);
    }
    if session.agent_pubkey != agent_pubkey || session.runtime != SessionRuntime::Managed || session.state != SessionState::Idle {
        return Ok(None);
    }
    // A remote session is a network hop further, which is what this avoids.
    if super::host::host_of(ctx, &session)?.is_some() {
        return Ok(None);
    }
    let Some(socket) = session.socket_path.as_deref() else { return Ok(None) };
    match axel_acp::socket::send(socket, &Command::Status) {
        Ok(Reply::Status { state: SessionState::Idle, pending: 0, queued: 0, .. }) => Ok(Some(session)),
        // Busy after all, or its host is gone (it ended idle a moment ago):
        // a cold start, which resumes the conversation.
        _ => Ok(None),
    }
}

/// Start a turn with `prompt` in a warm session; the seq of the prompt's own
/// item, after which the turn's are.  `None` when the session would not take
/// it at once, which leaves it as it was.
fn send_prompt(ctx: &Ctx, session: &axel_store::Session, prompt: &str) -> Result<Option<i64>> {
    let socket = session.socket_path.as_deref().expect("a warm session has a socket");
    let before = last_seq(ctx, &session.id)?;
    // Straight to the socket, not `ui::send_to_session`: a host that has just
    // gone must not be revived as another session than the one followed here.
    let command = Command::Prompt { text: prompt.to_string(), attachments: Vec::new(), steer: false, from: None };
    match axel_acp::socket::send(socket, &command) {
        Ok(Reply::Ok) => {}
        // A turn started between the status and the prompt: take the prompt
        // back and start cold rather than wait behind it.
        Ok(Reply::Queued { item_id } | Reply::Steering { item_id }) => {
            if let Err(e) = axel_acp::socket::send(socket, &Command::Dequeue { item_id }) {
                eprintln!("{}", dim(&t!("could not take the prompt back from the busy session: {error}", error = e)));
            }
            return Ok(None);
        }
        Ok(Reply::Error { .. } | Reply::Status { .. }) | Err(_) => return Ok(None),
    }
    let seq = prompt_item_seq(ctx, &session.id, before)?;
    assert!(seq > before, "the prompt's item comes after the items before it");
    Ok(Some(seq))
}

/// The seq of the first `UserMessage` after `after_seq`: the prompt just sent.
fn prompt_item_seq(ctx: &Ctx, session_id: &str, after_seq: i64) -> Result<i64> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(PROMPT_RECORD_WAIT_MAX_SECS) {
        let items = ctx.store.items(session_id, after_seq, ITEMS_PER_POLL_MAX)?;
        if let Some(item) = items.iter().find(|i| i.kind == ItemKind::UserMessage) {
            return Ok(item.seq);
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
    bail!("{}", t!("the session took the prompt but never recorded it; its log is in ~/.axel/logs/{session}.host.log", session = session_id))
}

/// The seq of the session's last item, 0 when it has none.
fn last_seq(ctx: &Ctx, session_id: &str) -> Result<i64> {
    let mut seq = 0i64;
    for _ in 0..ITEM_PAGES_MAX {
        let items = ctx.store.items(session_id, seq, ITEMS_PER_POLL_MAX)?;
        let Some(last) = items.last() else { return Ok(seq) };
        assert!(last.seq > seq, "items come in seq order, after the seq asked for");
        seq = last.seq;
        if items.len() < ITEMS_PER_POLL_MAX {
            return Ok(seq);
        }
    }
    bail!("{}", t!("session {session} holds more items than expedite reads", session = session_id))
}

/// `axel expedite --warm`: a spare session for the next expedite here,
/// started now, idle until a prompt takes it or its timeout ends it.
fn warm(request: Request) -> Result<()> {
    assert!(request.prompt.trim().is_empty(), "--warm takes no prompt");
    let keep = request.keep.unwrap_or(SPARE_KEEP_DEFAULT);
    if keep.is_zero() {
        bail!("{}", t!("a spare needs a --keep above 0: it waits for the next prompt"));
    }
    keep_for_hosts(keep);
    let mut ctx = Ctx::open()?;
    ctx.rex = None;
    let agent = pick_agent(&ctx, request.agent.as_deref())?;
    let cwd = current_dir()?;
    let started = Instant::now();
    if let Some(old) = read_spare() {
        let is_same_place = old.cwd == cwd && old.agent_pubkey == agent.pubkey;
        if is_same_place && warm_session(&ctx, &old.session_id, &agent.pubkey)?.is_some() {
            eprintln!("{}", dim(&t!("✓ a spare {agent} is already ready here", agent = agent.name)));
            return Ok(());
        }
        // One spare at a time: the one replaced would only wait out its timeout.
        if let Some(session) = ctx.store.get_session(&old.session_id)?.filter(|s| s.ended_at.is_none()) {
            super::thread::end_sessions(&ctx, &[session], SPARE_REPLACED_REASON)?;
        }
    }
    let thread = super::thread::record_thread(&ctx, t!("expedite: a spare, waiting for its prompt"), std::slice::from_ref(&cwd))?;
    // No prompt: the host opens the session and waits, idle.
    let session_id = super::ui::summon_with(&ctx, &thread.id, &format!("{}@local", agent.name), "", &[], false)?;
    write_spare(&Spare { session_id: session_id.clone(), cwd, agent_pubkey: agent.pubkey.clone(), keep_secs: keep.as_secs() })?;
    wait_until_idle(&ctx, &session_id)?;
    let took = format!("{:.1}s", started.elapsed().as_secs_f64());
    eprintln!(
        "{}",
        dim(&t!("✓ a spare {agent} is ready ({took}) · kept warm for {keep} · the next axel expedite \"…\" here takes it",
            agent = agent.name, took = took, keep = format_keep(keep)))
    );
    Ok(())
}

/// Wait for a new session to open and fall idle; an error if it ends first.
fn wait_until_idle(ctx: &Ctx, session_id: &str) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(START_WAIT_MAX_SECS) {
        let session = ctx.store.get_session(session_id)?.with_context(|| t!("no session {id}", id = session_id))?;
        if session.ended_at.is_some() {
            break;
        }
        if session.state == SessionState::Idle {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
    bail!("{}", t!("the session didn't start; its log is in ~/.axel/logs/{session}.host.log", session = session_id))
}

/// `--keep`: `0`, seconds, or a whole number of `s`, `m` or `h`; a day at most.
pub fn parse_keep(text: &str) -> std::result::Result<Duration, String> {
    let text = text.trim();
    let (digits, unit_secs) = match text.char_indices().last() {
        Some((i, 's')) => (&text[..i], 1u64),
        Some((i, 'm')) => (&text[..i], 60),
        Some((i, 'h')) => (&text[..i], 60 * 60),
        _ => (text, 1),
    };
    let Ok(count) = digits.parse::<u64>() else {
        return Err(t!("{value} is not a duration: try 0, 90s, 10m or 1h", value = format!("{text:?}")).to_string());
    };
    let keep = Duration::from_secs(count.saturating_mul(unit_secs));
    if keep > axel_acp::IDLE_EXIT_AFTER_MAX {
        return Err(t!("a session is kept warm for a day at most, not {value}", value = text).to_string());
    }
    Ok(keep)
}

/// `90s`, `10m`, `1h`: in the largest unit that divides it.
fn format_keep(keep: Duration) -> String {
    match keep.as_secs() {
        s if s >= 3600 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s >= 60 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// The agent named, or the one used most recently, or the only one.
fn pick_agent(ctx: &Ctx, asked: Option<&str>) -> Result<axel_store::Agent> {
    if let Some(name) = asked {
        return ctx.store.agent_by_name(name)?.with_context(|| t!("no agent named {name}", name = format!("{name:?}")));
    }
    let mut agents: Vec<axel_store::Agent> = ctx.store.list_agents()?.into_iter().filter(|a| a.archived_at.is_none()).collect();
    let recent = ctx.store.list_sessions(axel_store::SessionFilter { limit: Some(1), ..Default::default() })?;
    if let Some(at) = recent.first().and_then(|s| agents.iter().position(|a| a.pubkey == s.agent_pubkey)) {
        return Ok(agents.swap_remove(at));
    }
    match agents.len() {
        1 => Ok(agents.swap_remove(0)),
        0 => bail!("{}", t!("no agents yet: create one with `axel agent new <name> claude`")),
        _ => bail!("{}", t!("which agent? Pass --agent <name>")),
    }
}

/// The directory a new thread works in, resolved as a spare records it.
fn current_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context(t!("reading the current directory"))?;
    // Kept as found when it can't be resolved, as `record_thread` does.
    Ok(std::fs::canonicalize(&cwd).unwrap_or(cwd))
}

/// The prompt's first line, cut to a title.
fn title_of(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or(prompt).trim();
    let title = crate::cmd::text::fit(line, TITLE_CHARS_MAX);
    debug_assert!(axel_tui::util::text_width(&title) <= TITLE_CHARS_MAX);
    title
}

/// What the last expedite used, for `-c`.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Last {
    thread_id: String,
    /// `None` in a note from before sessions were kept warm.
    session_id: Option<String>,
    /// How long that session stays up idle.
    keep_secs: Option<u64>,
}

/// The spare `--warm` started, for the next expedite.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Spare {
    session_id: String,
    cwd: PathBuf,
    agent_pubkey: String,
    keep_secs: u64,
}

fn last_thread(ctx: &Ctx) -> Result<String> {
    let id = read_last().map(|l| l.thread_id).with_context(|| t!("nothing to continue yet: run `axel expedite \"…\"` first"))?;
    ctx.store.get_thread(&id)?.with_context(|| t!("the last expedite thread is gone; start a new one"))?;
    Ok(id)
}

fn read_last() -> Option<Last> {
    parse_last(&read_state_file(LAST_FILE)?)
}

/// The note as JSON, or as the bare thread id earlier versions wrote.
fn parse_last(text: &str) -> Option<Last> {
    if let Ok(last) = serde_json::from_str::<Last>(text) {
        return Some(last);
    }
    let id = text.trim();
    let is_bare_id = !id.is_empty() && !id.starts_with('{') && !id.contains(char::is_whitespace);
    is_bare_id.then(|| Last { thread_id: id.to_string(), session_id: None, keep_secs: None })
}

fn remember_last(last: &Last) {
    // A lost note only costs `-c` its default; the thread itself is stored.
    if let Ok(text) = serde_json::to_string(last) {
        let _ = std::fs::write(crate::paths::home().join(LAST_FILE), text);
    }
}

fn read_spare() -> Option<Spare> {
    // An unreadable note is no spare: the next expedite starts cold.
    serde_json::from_str(&read_state_file(SPARE_FILE)?).ok()
}

fn write_spare(spare: &Spare) -> Result<()> {
    let path = crate::paths::home().join(SPARE_FILE);
    std::fs::write(&path, serde_json::to_string(spare)?).with_context(|| t!("writing {path}", path = path.display()))
}

/// A small note under the Axel home; `None` when missing or unreadable.
fn read_state_file(name: &str) -> Option<String> {
    let file = std::fs::File::open(crate::paths::home().join(name)).ok()?;
    let mut text = String::new();
    file.take(STATE_FILE_BYTES_MAX).read_to_string(&mut text).ok()?;
    Some(text)
}

/// How the turn ended, for the summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Done,
    Cancelled,
    Failed,
    /// Still running after [`TURN_WAIT_MAX_SECS`], or left with a second Ctrl+C.
    Left,
}

/// Print what the session records after `from_seq` until its turn ends.
fn follow(ctx: &Ctx, session_id: &str, from_seq: i64) -> Result<Outcome> {
    let started = Instant::now();
    let mut printed: HashMap<String, usize> = HashMap::new();
    let mut answered: Vec<String> = Vec::new();
    let mut from_seq = from_seq;
    let mut cancelled = false;
    let mut out = std::io::stdout();
    loop {
        match INTERRUPTS.load(Ordering::SeqCst) {
            0 => {}
            1 if !cancelled => {
                cancelled = true;
                eprintln!("\n{}", dim(t!("cancelling… (Ctrl+C again to leave it running)")));
                if let Err(e) = super::ui::send_to_session(ctx, session_id, SendCommand::Cancel) {
                    eprintln!("{}", dim(&t!("could not cancel: {error}", error = format!("{e:#}"))));
                }
            }
            1 => {}
            _ => return Ok(Outcome::Left),
        }
        if started.elapsed() > Duration::from_secs(TURN_WAIT_MAX_SECS) {
            return Ok(Outcome::Left);
        }
        let items = ctx.store.items(session_id, from_seq, ITEMS_PER_POLL_MAX)?;
        if items.is_empty() && printed.is_empty() {
            // A host that failed to start ends its session with why: say so at once.
            let ended = ctx.store.get_session(session_id)?.filter(|s| s.ended_at.is_some());
            if let Some(session) = ended {
                let why = session.ended_reason.unwrap_or_default();
                bail!("{}", t!("the session ended before it answered: {reason}", reason = why));
            }
            if started.elapsed() > Duration::from_secs(START_WAIT_MAX_SECS) {
                bail!("{}", t!("the session didn't start; its log is in ~/.axel/logs/{session}.host.log", session = session_id));
            }
        }
        // Items grow in place while they stream: read again from the first
        // one still open, past the ones that are done.
        let mut still_open: Option<i64> = None;
        for item in &items {
            if let Some(end) = show(ctx, session_id, item, &mut printed, &mut answered, &mut out)? {
                let _ = out.flush();
                return Ok(end);
            }
            let open = matches!(item.status, Some(ItemStatus::Running | ItemStatus::Pending) | None) && item.kind != ItemKind::TurnEnd;
            if open && still_open.is_none() {
                still_open = Some(item.seq - 1);
            }
        }
        if let Some(last) = items.last() {
            from_seq = still_open.unwrap_or(last.seq);
        }
        let _ = out.flush();
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
}

/// Print `item` (or what's new in it); `Some` when it ends the turn.
fn show(
    ctx: &Ctx,
    session_id: &str,
    item: &ConversationItem,
    printed: &mut HashMap<String, usize>,
    answered: &mut Vec<String>,
    out: &mut std::io::Stdout,
) -> Result<Option<Outcome>> {
    match item.kind {
        ItemKind::AssistantMessage => {
            let text = item.content.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let done = printed.entry(item.id.clone()).or_insert(0);
            // Byte offsets of what was printed: text only grows, on char boundaries.
            if text.len() > *done && text.is_char_boundary(*done) {
                let _ = write!(out, "{}", &text[*done..]);
                *done = text.len();
            }
        }
        ItemKind::ToolCall => {
            if !printed.contains_key(&item.id) {
                printed.insert(item.id.clone(), 0);
                if let ItemBody::Tool { title, summary, .. } = axel_tui::item_body(item) {
                    let what = if summary.is_empty() { title } else { summary };
                    eprintln!("{}", dim(&format!("· {}", crate::cmd::text::fit(&what, 100))));
                }
            }
        }
        ItemKind::Permission | ItemKind::Question if item.status == Some(ItemStatus::Pending) && !answered.contains(&item.id) => {
            answered.push(item.id.clone());
            let _ = out.flush();
            answer(ctx, session_id, item)?;
        }
        ItemKind::Error if !printed.contains_key(&item.id) => {
            printed.insert(item.id.clone(), 0);
            let message = item.content.get("message").and_then(|v| v.as_str()).unwrap_or("");
            eprintln!("\x1b[31m✖ {message}\x1b[0m");
        }
        ItemKind::TurnEnd => {
            let reason = item.content.get("stop_reason").and_then(|v| v.as_str()).unwrap_or("");
            let _ = writeln!(out);
            return Ok(Some(match (item.status, reason) {
                (Some(ItemStatus::Cancelled), _) | (_, "cancelled") => Outcome::Cancelled,
                (Some(ItemStatus::Failed), _) => Outcome::Failed,
                _ => Outcome::Done,
            }));
        }
        _ => {}
    }
    Ok(None)
}

/// A question or permission, asked here: numbered options, an answer read
/// from the terminal.  Without a terminal there is no one to ask.
fn answer(ctx: &Ctx, session_id: &str, item: &ConversationItem) -> Result<()> {
    let ItemBody::Choice { prompt, options, multiple, .. } = axel_tui::item_body(item) else { return Ok(()) };
    eprintln!("\n\x1b[33m? {prompt}\x1b[0m");
    for (n, o) in options.iter().enumerate() {
        eprintln!("  {}. {}", n + 1, o.label);
    }
    if !std::io::stdin().is_terminal() {
        eprintln!("{}", dim(t!("no terminal to answer from: answer it in Axel (axel thread open)")));
        return Ok(());
    }
    let hint = if multiple.is_some() { t!("numbers, comma-separated:") } else { t!("number:") };
    let picked = loop {
        eprint!("{} ", dim(hint));
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let read = std::io::stdin().lock().take(u64::try_from(ANSWER_BYTES_MAX).unwrap_or(u64::MAX)).read_line(&mut line)?;
        if read == 0 {
            return Ok(());
        }
        let picks: Option<Vec<usize>> = line.split(',').map(|p| p.trim().parse::<usize>().ok().filter(|n| (1..=options.len()).contains(n)).map(|n| n - 1)).collect();
        match picks {
            Some(p) if !p.is_empty() && (multiple.is_some() || p.len() == 1) => break p,
            _ => eprintln!("{}", dim(&t!("pick 1 to {n}", n = options.len()))),
        }
    };
    let option_id = if multiple.is_some() {
        let ids: Vec<&str> = picked.iter().map(|i| options[*i].id.as_str()).collect();
        format!("answer:multi:{}", serde_json::to_string(&ids)?)
    } else {
        options[picked[0]].id.clone()
    };
    if let SendReply::Error { message } = super::ui::send_to_session(ctx, session_id, SendCommand::Answer { item_id: item.id.clone(), option_id })? {
        eprintln!("\x1b[31m✖ {message}\x1b[0m");
    }
    Ok(())
}

/// End the session; its conversation stays for `-c` and `axel thread open`.
fn stop(ctx: &Ctx, session_id: &str) {
    let session = match ctx.store.get_session(session_id) {
        Ok(Some(s)) if s.ended_at.is_none() => s,
        _ => return,
    };
    if let Err(e) = super::thread::end_sessions(ctx, &[session], DONE_REASON) {
        eprintln!("{}", dim(&t!("could not stop the session: {error}", error = format!("{e:#}"))));
    }
}

/// `✓ 3.7s · 1.2k tokens · ~$0.01 · kept warm for 10m`, then how to go on.
/// The tokens and cost are the whole session's, earlier turns included.
fn summary(ctx: &Ctx, turn: &Turn, elapsed: Duration, outcome: Outcome, kept: Duration) {
    let usage = ctx.store.usage(&turn.session_id).ok().flatten();
    let tokens = usage.as_ref().map_or(0, |u| u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens);
    let cost = usage.as_ref().map_or(0.0, |u| u.cost_usd_est);
    let glyph = match outcome {
        Outcome::Done => "✓",
        Outcome::Cancelled => "■",
        Outcome::Failed => "✖",
        Outcome::Left => "…",
    };
    let short: String = turn.thread_id.chars().take(8).collect();
    let took = format!("{:.1}s", elapsed.as_secs_f64());
    let (tokens, cost) = (axel_tui::util::format_tokens(tokens), format!("{cost:.2}"));
    let line = if kept.is_zero() {
        t!("{glyph} {took} · {tokens} tokens · ~${cost} · more: axel expedite -c \"…\" · all of it: axel thread open {thread}",
            glyph = glyph, took = took, tokens = tokens, cost = cost, thread = short)
    } else {
        t!("{glyph} {took} · {tokens} tokens · ~${cost} · kept warm for {keep} · more: axel expedite -c \"…\" · all of it: axel thread open {thread}",
            glyph = glyph, took = took, tokens = tokens, cost = cost, keep = format_keep(kept), thread = short)
    };
    eprintln!("{}", dim(&line));
}

fn dim(text: &str) -> String {
    if std::io::stderr().is_terminal() { format!("\x1b[2m{text}\x1b[0m") } else { text.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_the_prompts_first_line_cut_to_fit() {
        assert_eq!(title_of("fix the failing test\nthen push"), "fix the failing test");
        let long = "x".repeat(TITLE_CHARS_MAX * 2);
        assert!(axel_tui::util::text_width(&title_of(&long)) <= TITLE_CHARS_MAX);
        assert!(axel_tui::util::text_width(&title_of(&"修".repeat(TITLE_CHARS_MAX))) <= TITLE_CHARS_MAX, "wide characters count double");
    }

    #[test]
    fn keep_takes_seconds_minutes_and_hours_up_to_a_day() {
        assert_eq!(parse_keep("0"), Ok(Duration::ZERO));
        assert_eq!(parse_keep("90"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_keep("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_keep("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(parse_keep("24h"), Ok(axel_acp::IDLE_EXIT_AFTER_MAX));
        assert!(parse_keep("25h").is_err(), "past a day");
        assert!(parse_keep("86401").is_err(), "past a day, in seconds");
        assert!(parse_keep(&format!("{}h", u64::MAX)).is_err(), "overflow saturates, then is refused");
        for bad in ["", "m", "-1", "1.5m", "10 m", "ten", "1d", "1hm"] {
            assert!(parse_keep(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn keep_is_shown_in_the_largest_whole_unit() {
        assert_eq!(format_keep(Duration::from_secs(600)), "10m");
        assert_eq!(format_keep(Duration::from_secs(3600)), "1h");
        assert_eq!(format_keep(Duration::from_secs(5400)), "90m");
        assert_eq!(format_keep(Duration::from_secs(90)), "90s");
        assert_eq!(format_keep(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn the_last_note_reads_back_and_reads_the_bare_id_earlier_versions_wrote() {
        let last = Last { thread_id: "t1".into(), session_id: Some("s1".into()), keep_secs: Some(600) };
        assert_eq!(parse_last(&serde_json::to_string(&last).unwrap()), Some(last));
        assert_eq!(parse_last("t1\n"), Some(Last { thread_id: "t1".into(), session_id: None, keep_secs: None }));
        assert_eq!(parse_last(""), None);
        assert_eq!(parse_last("{\"broken\""), None, "a damaged note is no note");
    }
}
