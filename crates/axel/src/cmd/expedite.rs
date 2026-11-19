//! `axel expedite "…"`: the quickest way through Axel, for when there is
//! little time or little network.  One agent, a fresh context, no launcher,
//! window or panes: a managed session starts out of sight, its reply streams
//! here, and when the turn ends the session stops with its conversation
//! kept.  `-c` picks it up again; `axel thread open` shows it all.
//!
//! The session is an ordinary one (same host, store and policy), so nothing
//! here drives the harness: this only reads what the host records and, when
//! the agent asks something, answers it through the host's socket.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axel_i18n::t;
use axel_store::{ConversationItem, ItemKind, ItemStatus};
use axel_tui::model::ItemBody;
use axel_tui::{SendCommand, SendReply};

use super::Ctx;

/// How often the store is read for what the session wrote.
const POLL_MS: u64 = 50;
/// Most items read in one poll.
const ITEMS_PER_POLL_MAX: usize = 500;
/// A turn longer than this is left to run; expedite stops following it.
const TURN_WAIT_MAX_SECS: u64 = 60 * 60;
/// How long to wait for the session to record its first item.
const START_WAIT_MAX_SECS: u64 = 120;
/// Longest a thread's title is, cut from the prompt.
const TITLE_CHARS_MAX: usize = 60;
/// Where the last thread expedite used is kept, for `-c`.
const LAST_THREAD_FILE: &str = "expedite-last";
/// The reason the session ends with: its conversation resumes on the next message.
const DONE_REASON: &str = "expedite: turn done";
/// Largest answer line read from the terminal.
const ANSWER_BYTES_MAX: usize = 1024;

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
}

pub fn run(request: Request) -> Result<()> {
    let prompt = request.prompt.trim().to_string();
    if prompt.is_empty() {
        bail!("{}", t!("what should the agent do? `axel expedite \"fix the failing test\"`"));
    }
    let mut ctx = Ctx::open()?;
    // No window, no panes: without Rex the session host runs detached.
    ctx.rex = None;
    let agent = pick_agent(&ctx, request.agent.as_deref())?;
    let thread_id = match (request.thread, request.resume_last) {
        (Some(t), _) => super::ui::resolve_thread_id(&ctx, &t)?,
        (None, true) => last_thread(&ctx)?,
        (None, false) => {
            let cwd = std::env::current_dir().context(t!("reading the current directory"))?;
            super::thread::record_thread(&ctx, &title_of(&prompt), &[cwd])?.id
        }
    };
    remember_thread(&thread_id);
    let started = Instant::now();
    // `@local`: a remote host is a network hop further, which is what this avoids.
    let session_id = super::ui::summon_with(&ctx, &thread_id, &format!("{agent}@local"), &prompt, &[], false)?;
    // SAFETY: the handler only bumps an atomic counter.
    unsafe { libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t) };
    let outcome = follow(&ctx, &session_id);
    let elapsed = started.elapsed();
    stop(&ctx, &session_id);
    let outcome = outcome?;
    summary(&ctx, &session_id, &thread_id, elapsed, outcome);
    Ok(())
}

/// The agent named, or the one used most recently, or the only one.
fn pick_agent(ctx: &Ctx, asked: Option<&str>) -> Result<String> {
    if let Some(name) = asked {
        let agent = ctx.store.agent_by_name(name)?.with_context(|| t!("no agent named {name}", name = format!("{name:?}")))?;
        return Ok(agent.name);
    }
    let agents: Vec<axel_store::Agent> = ctx.store.list_agents()?.into_iter().filter(|a| a.archived_at.is_none()).collect();
    let recent = ctx.store.list_sessions(axel_store::SessionFilter { limit: Some(1), ..Default::default() })?;
    if let Some(agent) = recent.first().and_then(|s| agents.iter().find(|a| a.pubkey == s.agent_pubkey)) {
        return Ok(agent.name.clone());
    }
    match agents.as_slice() {
        [only] => Ok(only.name.clone()),
        [] => bail!("{}", t!("no agents yet: create one with `axel agent new <name> claude`")),
        _ => bail!("{}", t!("which agent? Pass --agent <name>")),
    }
}

/// The prompt's first line, cut to a title.
fn title_of(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or(prompt).trim();
    let title = crate::cmd::text::fit(line, TITLE_CHARS_MAX);
    debug_assert!(axel_tui::util::text_width(&title) <= TITLE_CHARS_MAX);
    title
}

fn last_thread(ctx: &Ctx) -> Result<String> {
    let path = crate::paths::home().join(LAST_THREAD_FILE);
    let id = std::fs::read_to_string(&path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let id = id.with_context(|| t!("nothing to continue yet: run `axel expedite \"…\"` first"))?;
    ctx.store.get_thread(&id)?.with_context(|| t!("the last expedite thread is gone; start a new one"))?;
    Ok(id)
}

fn remember_thread(thread_id: &str) {
    // A lost note only costs `-c` its default; the thread itself is stored.
    let _ = std::fs::write(crate::paths::home().join(LAST_THREAD_FILE), thread_id);
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

/// Print what the session records until its turn ends.
fn follow(ctx: &Ctx, session_id: &str) -> Result<Outcome> {
    let started = Instant::now();
    let mut printed: HashMap<String, usize> = HashMap::new();
    let mut answered: Vec<String> = Vec::new();
    let mut from_seq = 0i64;
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
        if items.is_empty() && printed.is_empty() && started.elapsed() > Duration::from_secs(START_WAIT_MAX_SECS) {
            bail!("{}", t!("the session didn't start; its log is in ~/.axel/logs/{session}.host.log", session = session_id));
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

/// `✓ 3.7s · 1.2k tokens · ~$0.01`, then how to go on.
fn summary(ctx: &Ctx, session_id: &str, thread_id: &str, elapsed: Duration, outcome: Outcome) {
    let usage = ctx.store.usage(session_id).ok().flatten();
    let tokens = usage.as_ref().map_or(0, |u| u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens);
    let cost = usage.as_ref().map_or(0.0, |u| u.cost_usd_est);
    let glyph = match outcome {
        Outcome::Done => "✓",
        Outcome::Cancelled => "■",
        Outcome::Failed => "✖",
        Outcome::Left => "…",
    };
    let short: String = thread_id.chars().take(8).collect();
    let took = format!("{:.1}s", elapsed.as_secs_f64());
    eprintln!(
        "{}",
        dim(&t!("{glyph} {took} · {tokens} tokens · ~${cost} · more: axel expedite -c \"…\" · all of it: axel thread open {thread}",
            glyph = glyph, took = took, tokens = axel_tui::util::format_tokens(tokens), cost = format!("{cost:.2}"), thread = short))
    );
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
}
