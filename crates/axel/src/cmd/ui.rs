//! The launcher: a floating centered pane inside Rex, or inline elsewhere.

use anyhow::{Context, Result};
use axel_tui::{Deps, Outcome, Screen, SendCommand, SendReply};
use axel_store::SessionRuntime;

use super::{Ctx, launch};
use crate::cli::ScreenArg;

/// Floating pane bounds as fractions of the window, centered.
/// The launcher is a short strip; the dashboard and inbox need room.
/// The prompt with the recent threads under it, in the upper part of the
/// window, not flush with the top.
const PROMPT_BOUNDS: [f64; 4] = [0.20, 0.10, 0.60, 0.62];
/// Share of that layer the prompt keeps; the recent threads get the rest.
const PROMPT_RATIO: f64 = 0.15;
/// The prompt's share while it shows the full launch form.
const PROMPT_RATIO_EXPANDED: f64 = 0.88;
/// Where an agent pane grows to after launch, and the standalone dashboard size.
const DASHBOARD_BOUNDS: [f64; 4] = [0.10, 0.08, 0.80, 0.84];
/// A thread docked into the window: what's already there keeps this share of
/// the width, and the conversation gets this share of the dock's height (the
/// composer below it fits itself to its text).
const DOCK_KEEP: f64 = 0.4;
const DOCK_STREAM: f64 = 0.85;
/// The pane the launcher was opened from, handed to the floating panes.
const ORIGIN_BLOCK_ENV: &str = "AXEL_ORIGIN_BLOCK";
/// Programs that sit at a prompt when nothing runs in them.
const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "dash", "ksh", "tcsh", "csh", "nu", "elvish", "xonsh"];
/// The composer pane's label; a split with it below is a docked thread.
const COMPOSE_LABEL: &str = "compose";
/// The prompt pane's label, which Rex shows as its title.
pub const PROMPT_LABEL: &str = "why not today?";
/// The usage HUD older versions opened beside the launcher; still closed by the toggle.
const USAGE_LABEL: &str = "usage";
pub const THREADS_LABEL: &str = "threads";
/// The tokens pane beside a composer: this many columns (its summary line
/// and a row per agent fit)…
const DOCK_USAGE_COLS: u16 = 34;
/// …and never more than this share of the row…
const DOCK_USAGE_MAX_SHARE: f64 = 0.35;
/// …and only when the composer keeps at least this many.
const DOCK_COMPOSE_MIN_COLS: u16 = 50;
/// The usage pane's label. Not one of the launcher's: `axel` leaves it open.
const TOKENS_LABEL: &str = "tokens";
/// The usage pane's size in cells: room for each agent's last hour, day,
/// all time and cost, and a few agents.
const TOKENS_CELLS: (f64, f64) = (48.0, 9.0);
/// Its bounds when our pane's size is unknown.
const TOKENS_FALLBACK: [f64; 4] = [0.66, 0.02, 0.32, 0.24];

/// Harness names the new-agent form cycles through: the builtins installed on
/// this machine, in order (all of them when none is, so the form still offers some).
pub fn harnesses() -> Vec<String> {
    let all = axel_store::Harness::BUILTIN;
    let installed: Vec<String> = all.iter().filter(|h| super::launch::which(h.program()).is_some()).map(|h| h.to_string()).collect();
    if installed.is_empty() { all.iter().map(|h| h.to_string()).collect() } else { installed }
}

/// `axel` with no arguments.
///
/// Inside Rex: create a floating layer running `axel ui`, then exit; the pane
/// carries on. Outside Rex: run the TUI right here.
pub fn open_launcher() -> Result<()> {
    let Some(env) = axel_rex::detect() else {
        return run_inline(None, None);
    };
    let rex = axel_rex::Rex::new(env);

    // Toggle: if Axel's panes are already open in this window, close them.
    let open = axel_layers(&rex)?;
    if !open.is_empty() {
        for layer in open {
            rex.close_layer(&layer).ok();
        }
        return Ok(());
    }

    let exe = std::env::current_exe().context("locating the axel binary")?;
    let exe = exe.to_string_lossy().into_owned();
    let cwd = std::env::current_dir().ok();
    // A window still called "Window 7ca9480b" takes the project's name.
    if let Some(name) = cwd.as_deref().and_then(project_label) {
        name_own_window(&rex, &name, &[]);
    }
    let mut env: Vec<(String, String)> = passthrough_env();
    // The pane `axel` was typed in: a thread docked from the launcher takes its place.
    env.push((ORIGIN_BLOCK_ENV.into(), rex.env().block.clone()));

    // 1. `prompt`: what are we building. Focused, in front.
    let prompt_cmd = vec![exe.clone(), "ui".into(), "--screen".into(), "launch".into()];
    let layer = rex
        .new_floating_terminal_with_focus(PROMPT_LABEL, PROMPT_BOUNDS, &prompt_cmd, cwd.as_deref(), &env, true)
        .context("opening the prompt pane in Rex")?;
    // 2. `threads`: recent threads, split in under the prompt, unfocused.
    if let Some(prompt) = layer.block_ids.first() {
        let threads_cmd = vec![exe, "ui".into(), "--screen".into(), "threads".into()];
        if let Err(e) = rex.split_below(prompt, THREADS_LABEL, &threads_cmd, PROMPT_RATIO, cwd.as_deref(), &env, false) {
            eprintln!("axel: could not open the threads pane: {e}");
        }
    }
    Ok(())
}

/// The split ratio that gives the prompt pane `want` rows, from both panes'
/// current rows (Rex sizes splits by ratio); the one-line default when the
/// sizes can't be read.  Half a row past `want`, so rounding lands on it.
fn prompt_ratio(rex: &axel_rex::Rex, prompt: &str, threads: &str, want: u16) -> f64 {
    let (Ok(p), Ok(t)) = (rex.terminal_size(prompt), rex.terminal_size(threads)) else { return PROMPT_RATIO };
    let total = f64::from(p.rows) + f64::from(t.rows);
    if total < 4.0 {
        return PROMPT_RATIO;
    }
    // Never smaller than the one-line size it opens at: growing only, so
    // typing the first line doesn't move it.
    ((f64::from(want) + 0.5) / total).clamp(PROMPT_RATIO, PROMPT_RATIO_EXPANDED)
}

/// `axel usage`: the tokens pane in the top-right corner of this Rex window,
/// or, when it's open, closed. Outside Rex, the same screen right here.
pub fn toggle_usage() -> Result<()> {
    let Some(env) = axel_rex::detect() else {
        return run_inline(Some(ScreenArg::Usage), None);
    };
    let rex = axel_rex::Rex::new(env);
    let open = floating_layers(&rex, &[TOKENS_LABEL])?;
    if !open.is_empty() {
        for layer in open {
            rex.close_layer(&layer).ok();
        }
        return Ok(());
    }
    let bounds = rex
        .terminal_size(&rex.env().block)
        .ok()
        .filter(|s| s.columns > 0 && s.rows > 0)
        .map_or(TOKENS_FALLBACK, |s| corner_bounds(s.columns, s.rows));
    let exe = std::env::current_exe().context("locating the axel binary")?.to_string_lossy().into_owned();
    let cmd = vec![exe, "ui".into(), "--screen".into(), "usage".into()];
    let cwd = std::env::current_dir().ok();
    rex.new_floating_terminal_with_focus(TOKENS_LABEL, bounds, &cmd, cwd.as_deref(), &passthrough_env(), false)
        .context("opening the usage pane in Rex")?;
    Ok(())
}

/// [`TOKENS_CELLS`] as window fractions, in the top-right corner, from a
/// pane of `columns` × `rows` (ours: the window is at least that big, so the
/// pane comes out that size or a little larger). One more row for its title bar.
fn corner_bounds(columns: u16, rows: u16) -> [f64; 4] {
    const MARGIN: f64 = 0.01;
    let w = (TOKENS_CELLS.0 / f64::from(columns)).clamp(0.18, 0.6);
    let h = ((TOKENS_CELLS.1 + 1.0) / f64::from(rows)).clamp(0.12, 0.5);
    [(1.0 - w - MARGIN).max(0.0), MARGIN * 2.0, w, h]
}

/// Floating layers in the current window whose first block is one of ours.
fn axel_layers(rex: &axel_rex::Rex) -> Result<Vec<String>> {
    floating_layers(rex, &[PROMPT_LABEL, "prompt", USAGE_LABEL, "axel"])
}

/// Floating layers in the current window whose first block has one of `labels`.
fn floating_layers(rex: &axel_rex::Rex, labels: &[&str]) -> Result<Vec<String>> {
    let view = rex.view()?;
    let active = view.get("active_window_id").and_then(|w| w.as_str()).unwrap_or("");
    let mut out = Vec::new();
    for window in view.get("windows").and_then(|w| w.as_array()).into_iter().flatten() {
        if window.get("window_id").and_then(|w| w.as_str()) != Some(active) {
            continue;
        }
        for layer in window.get("layers").and_then(|l| l.as_array()).into_iter().flatten() {
            if layer.get("kind").and_then(|k| k.as_str()) != Some("floating") {
                continue;
            }
            let label = layer.pointer("/blocks/0/label").and_then(|l| l.as_str()).unwrap_or("");
            if labels.contains(&label)
                && let Some(id) = layer.get("layer_id").and_then(|i| i.as_str()) {
                    out.push(id.to_string());
                }
        }
    }
    Ok(out)
}

/// `axel ui`: run the TUI in this terminal. On a launch request, perform it and,
/// when running inside a Rex floating layer, close that layer last.
pub fn run_inline(screen: Option<ScreenArg>, thread: Option<String>) -> Result<()> {
    let ctx = Ctx::open()?;
    let owner = ctx.owner()?;
    let owner_pk = owner.public().to_string();
    super::reconcile::run(&ctx).ok();
    // Remote hosts still working for us (e.g. after a reboot) resume syncing.
    super::host::resume_followers(&ctx);

    let initial = match (screen, thread) {
        (Some(ScreenArg::Thread), Some(id)) => {
            let id = resolve_thread_id(&ctx, &id)?;
            if let Some(rex) = &ctx.rex {
                let block = rex.env().block.clone();
                label_block_with_thread(&ctx, rex, &block, &id);
            }
            Screen::Thread(id)
        }
        (Some(ScreenArg::Thread), None) => anyhow::bail!("--screen thread needs --thread ID"),
        (other, _) => initial_screen(other)?,
    };
    let _ = &owner_pk;

    let cwd = std::env::current_dir().context("reading current directory")?;
    let send = |session_id: &str, cmd: SendCommand| -> Result<SendReply> { send_to_session(&ctx, session_id, cmd) };
    let summon = |thread_id: &str, agent: &str, text: &str, files: &[std::path::PathBuf]| -> Result<String> { summon(&ctx, thread_id, agent, text, files) };
    let runtime_label = |h: &axel_store::Harness| launch::runtime_label(h);
    // From the floating launcher, a thread docks into the window under it:
    // the conversation above, the composer below, and the launcher closes.
    // Elsewhere it opens as its own tab.
    let open_thread = |tid: &str| -> Result<()> {
        if let Some(rex) = &ctx.rex
            && layer_of_block(rex, &rex.env().block)?.is_some()
        {
            return dock_thread(&ctx, rex, tid);
        }
        open_split_tab(&ctx, tid)?;
        close_own_layer(&ctx);
        Ok(())
    };
    let open_thread_ref: Option<&axel_tui::OpenThreadFn> = if ctx.rex.is_some() { Some(&open_thread) } else { None };
    // The prompt and the recent threads under it hand focus to each other.
    let peer = ctx.rex.as_ref().and_then(|rex| {
        let me = rex.env().block.clone();
        let label = match block_label(rex, &me).as_deref() {
            Some(PROMPT_LABEL) => THREADS_LABEL,
            Some(THREADS_LABEL) => PROMPT_LABEL,
            _ => return None,
        };
        sibling_block(rex, &me, label).map(|peer| (me, peer, label == THREADS_LABEL))
    });
    let focus_peer = || {
        if let (Some(rex), Some((_, peer, _))) = (&ctx.rex, &peer) {
            let _ = rex.api_call("session.focus_block", serde_json::json!({ "block_id": peer }));
        }
    };
    let fit_prompt = |size: axel_tui::PromptSize| {
        if let (Some(rex), Some((me, threads, true))) = (&ctx.rex, &peer) {
            let ratio = match size {
                axel_tui::PromptSize::Form => PROMPT_RATIO_EXPANDED,
                axel_tui::PromptSize::Rows(want) => prompt_ratio(rex, me, threads, want),
            };
            let _ = rex.api_call(
                "session.resize_pane",
                serde_json::json!({ "direction": "vertical", "before_block_id": me, "after_block_id": threads, "ratio": ratio }),
            );
        }
    };
    let manage_thread = |tid: &str, op: axel_tui::ThreadOp| -> Result<()> { super::thread::set_state(&ctx, tid, op) };
    let focus_peer_ref: Option<&dyn Fn()> = if peer.is_some() { Some(&focus_peer) } else { None };
    let fit_prompt_ref: Option<&dyn Fn(axel_tui::PromptSize)> = if matches!(peer, Some((_, _, true))) { Some(&fit_prompt) } else { None };
    // A launch turns the prompt into the agent or thread pane: the threads pane goes.
    let close_threads_pane = || {
        if let (Some(rex), Some((_, threads, true))) = (&ctx.rex, &peer) {
            let _ = rex.api_call("block.close", serde_json::json!({ "block_id": threads }));
        }
    };
    let deps = Deps {
        store: &ctx.store,
        keystore: &ctx.keystore,
        cwd,
        harnesses: harnesses(),
        send: &send,
        summon: &summon,
        runtime_label: &runtime_label,
        open_thread: open_thread_ref,
        focus_peer: focus_peer_ref,
        fit_prompt: fit_prompt_ref,
        manage_thread: Some(&manage_thread),
    };
    match axel_tui::run(deps, initial)? {
        Outcome::Quit => {
            close_own_layer(&ctx);
            Ok(())
        }
        Outcome::FocusSession(session_id) => {
            if let (Some(rex), Some(session)) = (&ctx.rex, ctx.store.get_session(&session_id)?)
                && let Some(w) = session.rex_window_id.as_deref()
            {
                rex.focus_window(w).ok();
            }
            close_own_layer(&ctx);
            Ok(())
        }
        Outcome::OpenLog(session_id) => {
            if let (Some(rex), Some(session)) = (&ctx.rex, ctx.store.get_session(&session_id)?)
                && let Some(w) = session.rex_window_id.as_deref()
            {
                rex.focus_window(w).ok();
            } else {
                eprintln!("axel: log at {}", super::session::log_path(&session_id).display());
            }
            Ok(())
        }
        Outcome::Launch(req) => {
            close_threads_pane();
            // An agent that runs on a host: the thread is recorded here, the
            // agent starts there, and this pane becomes the thread view.
            if let super::host::Placement::Host(h) = super::host::placement(&ctx, &req.agent_pubkey, None)? {
                let agent = ctx.store.agent_by_pubkey(&req.agent_pubkey)?.context("agent vanished")?;
                let (thread_id, _) = super::host::new_thread_remote(&ctx, &h.label, &req.title, &req.projects, &agent.name, &req.attachments)?;
                return open_thread_here(&ctx, &thread_id);
            }
            // Inside Rex the launcher pane becomes the agent pane (pane runtime)
            // or the thread view (managed runtime).
            let opts = launch::LaunchOptions { target: launch::Target::Here, ..Default::default() };
            match launch::launch(&ctx, req, opts)? {
                launch::Launched::Managed { thread_id, host_spec: Some(spec), .. } => {
                    // Outside Rex: this terminal becomes the host; the view is `axel thread open`.
                    eprintln!("axel: hosting managed session; open the thread with `axel thread open {thread_id}`");
                    launch::exec_inline(spec)
                }
                launch::Launched::Managed { thread_id, .. } => open_thread_here(&ctx, &thread_id),
                launch::Launched::RexWindow { window_id, .. } => {
                    eprintln!("axel: launched in Rex window {window_id}");
                    close_own_layer(&ctx);
                    Ok(())
                }
                launch::Launched::Inline { spec, label, thread_id, .. } => {
                    if let Some(rex) = &ctx.rex {
                        let block = rex.env().block.clone();
                        if let Ok(Some(layer)) = layer_of_block(rex, &block) {
                            rex.resize_layer(&layer, DASHBOARD_BOUNDS).ok();
                        }
                        rex.set_block_label(&block, &label).ok();
                        name_window_after_thread(&ctx, rex, &thread_id);
                    }
                    launch::exec_inline(spec)
                }
            }
        }
    }
}

/// After a launch, `thread_id`'s view: from the floating launcher, docked into
/// the window under it; in a plain pane, the stream here with the composer
/// split in below; outside Rex, the thread screen.
fn open_thread_here(ctx: &Ctx, thread_id: &str) -> Result<()> {
    if let Some(rex) = &ctx.rex {
        let block = rex.env().block.clone();
        if let Ok(Some(_)) = layer_of_block(rex, &block) {
            return dock_thread(ctx, rex, thread_id);
        }
        label_block_with_thread(ctx, rex, &block, thread_id);
        name_window_after_thread(ctx, rex, thread_id);
        return split_here(ctx, rex, thread_id);
    }
    let exe = std::env::current_exe()?;
    let spec = axel_config::launch::LaunchSpec {
        program: exe.to_string_lossy().into_owned(),
        args: vec!["ui".into(), "--screen".into(), "thread".into(), "--thread".into(), thread_id.to_string()],
        env: vec![],
    };
    launch::exec_inline(spec)
}

/// Explicit screen, else: inbox when something needs you, dashboard when there
/// are threads, launcher on a fresh install.
fn initial_screen(screen: Option<ScreenArg>) -> Result<Screen> {
    Ok(match screen {
        Some(ScreenArg::Launch) => Screen::Launcher,
        Some(ScreenArg::Inbox) => Screen::Inbox,
        Some(ScreenArg::Dashboard) => Screen::Dashboard,
        Some(ScreenArg::Thread) => Screen::Dashboard,
        Some(ScreenArg::Threads) => Screen::Threads,
        Some(ScreenArg::Usage) => Screen::Usage,
        None => {
            let ctx = Ctx::open()?;
            let owner_pk = ctx.owner()?.public().to_string();
            let pending = ctx.store.pending_requests()?.len();
            let unread = ctx.store.unread_count(&owner_pk)?;
            let has_threads = !ctx.store.list_threads(axel_store::ThreadFilter { limit: Some(1), ..Default::default() })?.is_empty();
            if pending > 0 || unread > 0 { Screen::Inbox } else if has_threads { Screen::Dashboard } else { Screen::Launcher }
        }
    })
}

/// If this process runs in a Rex floating layer, close that layer (which ends us).
pub(crate) fn close_own_layer(ctx: &Ctx) {
    let Some(rex) = &ctx.rex else { return };
    let block = rex.env().block.clone();
    match layer_of_block(rex, &block) {
        Ok(Some(layer_id)) => {
            if let Err(e) = rex.close_layer(&layer_id) {
                eprintln!("axel: could not close floating layer: {e}");
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("axel: could not inspect session: {e}"),
    }
}

/// A block's label, from `session.view`.
fn block_label(rex: &axel_rex::Rex, block_id: &str) -> Option<String> {
    let view = rex.view().ok()?;
    blocks_in_view(&view).into_iter().find_map(|(_, b)| {
        (b.get("block_id").and_then(|i| i.as_str()) == Some(block_id)).then(|| b.get("label").and_then(|l| l.as_str()).map(String::from)).flatten()
    })
}

/// The block labelled `label` in the same layer as `block_id`.
fn sibling_block(rex: &axel_rex::Rex, block_id: &str, label: &str) -> Option<String> {
    let view = rex.view().ok()?;
    let blocks = blocks_in_view(&view);
    let (layer, _) = blocks.iter().find(|(_, b)| b.get("block_id").and_then(|i| i.as_str()) == Some(block_id))?;
    blocks.iter().find_map(|(l, b)| {
        let id = b.get("block_id")?.as_str()?;
        (l == layer && id != block_id && b.get("label").and_then(|x| x.as_str()) == Some(label)).then(|| id.to_string())
    })
}

/// Every block in the session view with its layer id.
fn blocks_in_view(view: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    for window in view.get("windows").and_then(|w| w.as_array()).into_iter().flatten() {
        for layer in window.get("layers").and_then(|l| l.as_array()).into_iter().flatten() {
            let id = layer.get("layer_id").and_then(|i| i.as_str()).unwrap_or("").to_string();
            for b in layer.get("blocks").and_then(|b| b.as_array()).into_iter().flatten() {
                out.push((id.clone(), b.clone()));
            }
        }
    }
    out
}

/// Find the floating layer that contains `block_id`, from `session.view`.
pub fn layer_of_block(rex: &axel_rex::Rex, block_id: &str) -> Result<Option<String>> {
    Ok(locate_block(rex, block_id)?.and_then(|(_, layer)| layer))
}

/// The window that contains `block_id`.
pub fn window_of_block(rex: &axel_rex::Rex, block_id: &str) -> Result<Option<String>> {
    Ok(locate_block(rex, block_id)?.map(|(w, _)| w))
}

/// (window_id, floating layer_id if any) for a block.
fn locate_block(rex: &axel_rex::Rex, block_id: &str) -> Result<Option<(String, Option<String>)>> {
    let view = rex.view()?;
    let windows = view.get("windows").and_then(|w| w.as_array()).cloned().unwrap_or_default();
    for window in windows {
        let window_id = window.get("window_id").and_then(|i| i.as_str()).unwrap_or("").to_string();
        let layers = window.get("layers").and_then(|l| l.as_array()).cloned().unwrap_or_default();
        for layer in layers {
            let floating = layer.get("kind").and_then(|k| k.as_str()) == Some("floating");
            let blocks = layer.get("blocks").and_then(|b| b.as_array()).cloned().unwrap_or_default();
            if blocks.iter().any(|b| b.get("block_id").and_then(|i| i.as_str()) == Some(block_id)) {
                let layer_id = floating.then(|| layer.get("layer_id").and_then(|i| i.as_str()).map(String::from)).flatten();
                return Ok(Some((window_id, layer_id)));
            }
        }
    }
    Ok(None)
}

/// `axel thread split`: a Rex window with the stream above and the composer
/// below; the composer gets focus.  Outside Rex, fall back to the full view.
pub fn open_split(thread_id: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    if ctx.rex.is_none() {
        drop(ctx);
        return run_inline(Some(ScreenArg::Thread), Some(thread_id.to_string()));
    }
    open_split_tab(&ctx, thread_id)
}

/// `axel thread open` inside Rex: this pane becomes the stream with the
/// composer below; outside Rex, the full in-TUI view.
pub fn open_here(thread_id: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    let Some(rex) = ctx.rex.as_ref() else {
        drop(ctx);
        return run_inline(Some(ScreenArg::Thread), Some(thread_id.to_string()));
    };
    let block = rex.env().block.clone();
    label_block_with_thread(&ctx, rex, &block, thread_id);
    split_here(&ctx, rex, thread_id)
}

/// Split this pane: the composer below (told which block streams), then
/// stream the conversation here until the composer closes this pane.
fn split_here(ctx: &Ctx, rex: &axel_rex::Rex, thread_id: &str) -> Result<()> {
    let me = rex.env().block.clone();
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "axel".into());
    let compose: Vec<String> = vec![exe, "thread".into(), "compose".into(), thread_id.to_string()];
    let mut env = passthrough_env();
    env.push(("AXEL_STREAM_BLOCK".into(), me.clone()));
    let usage = usage_command();
    let size = rex.terminal_size(&me).ok();
    let width = size.as_ref().map(|s| s.columns);
    let ids = rex.split_below_beside(&me, COMPOSE_LABEL, &compose, usage_beside(&usage, width).as_ref(), DOCK_STREAM, None, &env, true)?;
    if let Some(compose) = ids.first() {
        settle_thread_row(rex, &me, compose, ids.get(1).map(String::as_str), size.map(|s| s.rows), width);
    }
    let owner_pk = ctx.owner()?.public().to_string();
    axel_tui::run_stream(&ctx.store, &owner_pk, thread_id)
}

/// Dock `thread_id` into the window this floating launcher sits over, the
/// conversation above and the composer below.  It goes where the shell `axel`
/// was typed in is, so nothing else moves: that shell is taken out of the
/// window, still running, and comes back when the composer closes.  When the
/// shell is busy (or unknown), the thread docks as a column on the right.
/// Then Axel's floating panes close, this one last (which ends this process).
fn dock_thread(ctx: &Ctx, rex: &axel_rex::Rex, thread_id: &str) -> Result<()> {
    let me = rex.env().block.clone();
    let thread = ctx.store.get_thread(thread_id)?.with_context(|| format!("no thread {thread_id}"))?;
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "axel".into());
    let stream: Vec<String> = vec![exe.clone(), "thread".into(), "stream".into(), thread.id.clone()];
    let compose: Vec<String> = vec![exe, "thread".into(), "compose".into(), thread.id.clone()];
    let title = axel_tui::util::ellipsize(&thread.title, 48);
    let cwd = std::env::current_dir().ok();
    let mut env = passthrough_env();
    let usage = usage_command();
    let ids = if let Some(origin) = reusable_origin(rex, &me) {
        env.push((ORIGIN_BLOCK_ENV.into(), origin.clone()));
        // The shell leaves once the thread is in: the column takes its width.
        let size = rex.terminal_size(&origin).ok();
        let width = size.as_ref().map(|s| s.columns);
        let ids = rex.split_right_stacked(&origin, (&title, &stream), (COMPOSE_LABEL, &compose), usage_beside(&usage, width).as_ref(), 0.5, DOCK_STREAM, cwd.as_deref(), &env, true)?;
        let _ = rex.api_call("session.detach_block", serde_json::json!({ "block_id": origin }));
        settle_thread_row(rex, &ids[0], &ids[1], ids.get(2).map(String::as_str), size.map(|s| s.rows), width);
        ids
    } else if let Some(anchor) = rex.view().ok().and_then(|v| tiled_root(&v, &me)) {
        let width = window_columns(rex, &me).map(|w| (f64::from(w) * (1.0 - DOCK_KEEP)) as u16);
        let ids = rex.split_right_stacked(&anchor, (&title, &stream), (COMPOSE_LABEL, &compose), usage_beside(&usage, width).as_ref(), DOCK_KEEP, DOCK_STREAM, cwd.as_deref(), &env, true)?;
        settle_thread_row(rex, &ids[0], &ids[1], ids.get(2).map(String::as_str), None, width);
        ids
    } else {
        // A window with nothing tiled in it: open the thread in its own tab.
        open_split_tab(ctx, thread_id)?;
        close_axel_layers(ctx, rex);
        return Ok(());
    };
    let _ = rex.api_call("session.focus_block", serde_json::json!({ "block_id": ids[1] }));
    name_window_after_thread(ctx, rex, thread_id);
    close_axel_layers(ctx, rex);
    Ok(())
}

/// The pane `axel` was typed in, when the thread can take its place: in the
/// launcher's window, and a shell back at its prompt.
fn reusable_origin(rex: &axel_rex::Rex, me: &str) -> Option<String> {
    let origin = std::env::var(ORIGIN_BLOCK_ENV).ok().filter(|o| !o.is_empty() && o != me)?;
    let window = window_of_block(rex, me).ok().flatten()?;
    (window_of_block(rex, &origin).ok().flatten()? == window && rex.process(&origin).is_ok_and(|p| is_idle_shell(&p))).then_some(origin)
}

/// From `com.superlogical.terminal.process`: the terminal's foreground
/// program is a shell, so it's at its prompt with nothing running.
fn is_idle_shell(process: &serde_json::Value) -> bool {
    let Some(fg) = process.get("foreground") else { return false };
    let name = ["argv0", "name"].iter().find_map(|k| fg.get(*k).and_then(|v| v.as_str())).unwrap_or("");
    let name = name.trim_start_matches('-').rsplit('/').next().unwrap_or("");
    SHELLS.contains(&name)
}

/// Put the shell a docked thread replaced back in its place: beside the
/// stream (or the composer), so when those two close it fills their space.
fn restore_origin(rex: &axel_rex::Rex, origin: &str, stream: Option<&str>) {
    let anchor = stream.map(String::from).unwrap_or_else(|| rex.env().block.clone());
    let moved = rex.api_call(
        "session.move_block",
        serde_json::json!({ "block_id": origin, "anchor_block_id": anchor, "direction": "horizontal", "side": "before" }),
    );
    if moved.is_ok() {
        let _ = rex.api_call("session.focus_block", serde_json::json!({ "block_id": origin }));
    }
}

/// Close every Axel floating pane in this window, this process's own last.
fn close_axel_layers(ctx: &Ctx, rex: &axel_rex::Rex) {
    let own = layer_of_block(rex, &rex.env().block).ok().flatten();
    for layer in axel_layers(rex).unwrap_or_default() {
        if Some(&layer) != own.as_ref() {
            rex.close_layer(&layer).ok();
        }
    }
    close_own_layer(ctx);
}

/// An agent's session pane is labelled `<agent> · <thread> · log`.
pub const AGENT_PANE_SUFFIX: &str = " · log";
/// The window holding `block` and its tiled layer, from `session.view`.
fn tiled_layer_of<'a>(view: &'a serde_json::Value, block: &str) -> Option<&'a serde_json::Value> {
    let has = |l: &serde_json::Value| l.get("blocks").and_then(|b| b.as_array()).is_some_and(|bs| bs.iter().any(|b| b.get("block_id").and_then(|i| i.as_str()) == Some(block)));
    view.get("windows")?.as_array()?.iter().find_map(|w| {
        let layers = w.get("layers")?.as_array()?;
        layers.iter().any(has).then(|| layers.iter().find(|l| l.get("kind").and_then(|k| k.as_str()) == Some("tiled"))).flatten()
    })
}

/// The id at the root of the tiled layout of `block`'s window: its one block,
/// or the split that holds everything.
fn tiled_root(view: &serde_json::Value, block: &str) -> Option<String> {
    let root = tiled_layer_of(view, block)?.get("layout")?;
    root.get("block_id").or_else(|| root.pointer("/split/split_id")).and_then(|i| i.as_str()).map(String::from)
}

/// The block above `block` in a top/bottom split: the stream a composer serves.
fn block_above(view: &serde_json::Value, block: &str) -> Option<String> {
    let layout = tiled_layer_of(view, block)?.get("layout")?;
    find_split(layout, &|_, after| after == block).map(|(before, _)| before).or_else(|| above_row(layout, block))
}

/// The block above a row whose left block is `block` (a composer with the
/// tokens pane beside it).
fn above_row(node: &serde_json::Value, block: &str) -> Option<String> {
    let split = node.get("split")?;
    let id = |n: &serde_json::Value| n.get("block_id").and_then(|i| i.as_str()).map(String::from);
    if split.get("direction").and_then(|d| d.as_str()) == Some("vertical")
        && let (Some(before), Some(row)) = (split.get("before").and_then(id), split.get("after").and_then(|a| a.get("split")))
        && row.get("direction").and_then(|d| d.as_str()) == Some("horizontal")
        && row.get("before").and_then(id).as_deref() == Some(block)
    {
        return Some(before);
    }
    above_row(split.get("before")?, block).or_else(|| above_row(split.get("after")?, block))
}

/// The first vertical split, depth first, whose halves are two blocks
/// `(before, after)` that `want` accepts.
fn find_split(node: &serde_json::Value, want: &dyn Fn(&str, &str) -> bool) -> Option<(String, String)> {
    let split = node.get("split")?;
    let id = |n: &serde_json::Value| n.get("block_id").and_then(|i| i.as_str()).map(String::from);
    if split.get("direction").and_then(|d| d.as_str()) == Some("vertical")
        && let (Some(before), Some(after)) = (split.get("before").and_then(id), split.get("after").and_then(id))
        && want(&before, &after)
    {
        return Some((before, after));
    }
    ["before", "after"].iter().find_map(|side| find_split(split.get(*side)?, want))
}

/// A new Rex tab with the stream above and the composer below; the composer gets focus.
fn open_split_tab(ctx: &Ctx, thread_id: &str) -> Result<()> {
    let rex = ctx.rex.as_ref().context("not inside Rex")?;
    let thread = ctx.store.get_thread(thread_id)?.with_context(|| format!("no thread {thread_id}"))?;
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "axel".into());
    let stream: Vec<String> = vec![exe.clone(), "thread".into(), "stream".into(), thread.id.clone()];
    let compose: Vec<String> = vec![exe, "thread".into(), "compose".into(), thread.id.clone()];
    let label = axel_tui::util::ellipsize(&thread.title, 40);
    let usage = usage_command();
    let width = window_columns(rex, &rex.env().block);
    let w = rex.new_window_stacked(&label, ("thread", &stream), (COMPOSE_LABEL, &compose), usage_beside(&usage, width).as_ref(), 0.8, None, &passthrough_env(), true)?;
    if let [stream, compose, rest @ ..] = &w.block_ids[..] {
        settle_thread_row(rex, stream, compose, rest.first().map(String::as_str), None, width);
    }
    if let Some(b) = w.block_ids.get(1) {
        let _ = rex.api_call("session.focus_block", serde_json::json!({ "block_id": b }));
    }
    Ok(())
}

/// `axel thread stream`: the top pane.
pub fn run_stream(thread_id: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    // Its runs of tool calls are `axel-links://` links: make sure something opens them.
    super::links::ensure_installed();
    let owner_pk = ctx.owner()?.public().to_string();
    axel_tui::run_stream(&ctx.store, &owner_pk, thread_id)
}

/// `axel thread compose`: the bottom pane.  Leaving it closes its window, so
/// the stream above goes with it.
pub fn run_compose(thread_id: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    let cwd = std::env::current_dir().context("reading current directory")?;
    let send = |session_id: &str, cmd: SendCommand| -> Result<SendReply> { send_to_session(&ctx, session_id, cmd) };
    let summon = |thread_id: &str, agent: &str, text: &str, files: &[std::path::PathBuf]| -> Result<String> { summon(&ctx, thread_id, agent, text, files) };
    let runtime_label = |h: &axel_store::Harness| launch::runtime_label(h);
    let deps = Deps { store: &ctx.store, keystore: &ctx.keystore, cwd, harnesses: harnesses(), send: &send, summon: &summon, runtime_label: &runtime_label, open_thread: None, focus_peer: None, fit_prompt: None, manage_thread: None };
    let stream = ctx.rex.as_ref().and_then(stream_block);
    // The tokens pane opened beside us, in the same row, if there is one.
    let usage = ctx.rex.as_ref().and_then(|rex| rex.view().ok().and_then(|v| block_beside(&v, &rex.env().block, TOKENS_LABEL)));
    if let (Some(rex), Some(stream), Some(usage)) = (&ctx.rex, &stream, &usage) {
        fit_usage_pane(rex, usage, stream);
    }
    let usage_fit = usage.clone();
    let fit = |want: u16, now: u16| {
        if let (Some(rex), Some(stream)) = (&ctx.rex, &stream) {
            fit_compose_pane(rex, stream, want, now);
            if let Some(usage) = &usage_fit {
                fit_usage_pane(rex, usage, stream);
            }
        }
    };
    // A docked thread stands in for a shell: leaving brings that shell back.
    let origin = std::env::var(ORIGIN_BLOCK_ENV).ok().filter(|o| !o.is_empty());
    let leave = move |rex: &axel_rex::Rex, stream: Option<&str>| {
        if let Some(usage) = &usage {
            let _ = rex.api_call("block.close", serde_json::json!({ "block_id": usage }));
        }
        if let Some(origin) = &origin {
            restore_origin(rex, origin, stream);
        }
        // The stream above goes too; this pane closes as its program exits.
        // Only those two blocks: the window may hold the user's other panes.
        if let Some(stream) = stream {
            let _ = rex.api_call("block.close", serde_json::json!({ "block_id": stream }));
        }
    };
    // Closing the pane (its × in Rex) hangs us up, about a second before we're
    // killed: leave right away, from a thread of our own.
    if ctx.rex.is_some() {
        let (leave, stream) = (leave.clone(), stream.clone());
        on_hangup(move || {
            if let Some(env) = axel_rex::detect() {
                leave(&axel_rex::Rex::new(env), stream.as_deref());
            }
        });
    }
    // A question an agent asks you floats in front of the thread, centered.
    let ask = |item_id: &str| {
        if let Some(rex) = &ctx.rex {
            open_question(rex, thread_id, item_id);
        }
    };
    // ^L: the agent's session log, minimized (detached) until asked for.
    let toggle_log = |session_id: &str| {
        if let (Some(rex), Ok(Some(session))) = (&ctx.rex, ctx.store.get_session(session_id))
            && let Some(block) = session.rex_block_id.as_deref()
        {
            let beside = stream.clone().unwrap_or_else(|| rex.env().block.clone());
            toggle_agent_pane(rex, block, &beside);
        }
    };
    let res = axel_tui::run_compose(deps, thread_id, &fit, &ask, &toggle_log);
    if let Some(rex) = &ctx.rex {
        leave(rex, stream.as_deref());
    }
    res
}

/// What the tokens pane beside a composer runs.
fn usage_command() -> Vec<String> {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "axel".into());
    vec![exe, "ui".into(), "--screen".into(), "usage".into()]
}

/// The tokens pane for a composer row `columns` wide, if it has room:
/// created with the composer, in the same layout call, so they appear together.
fn usage_beside(command: &[String], columns: Option<u16>) -> Option<axel_rex::Beside<'_>> {
    Some(axel_rex::Beside { label: TOKENS_LABEL, command, keep: usage_keep(columns?)? })
}

/// The share of a composer row `columns` wide that the composer keeps
/// beside a tokens pane [`DOCK_USAGE_COLS`] wide (at most
/// [`DOCK_USAGE_MAX_SHARE`] of it); `None` when the composer would be left
/// under [`DOCK_COMPOSE_MIN_COLS`].
fn usage_keep(columns: u16) -> Option<f64> {
    let total = f64::from(columns);
    let usage = f64::from(DOCK_USAGE_COLS).min(total * DOCK_USAGE_MAX_SHARE);
    (total - usage >= f64::from(DOCK_COMPOSE_MIN_COLS)).then(|| (total - usage) / total)
}

/// How many columns the window holding `block` is wide, from a tiled
/// block's columns and its share of the window's width.
fn window_columns(rex: &axel_rex::Rex, block: &str) -> Option<u16> {
    let view = rex.view().ok()?;
    let layout = tiled_layer_of(&view, block)?.get("layout")?;
    fn first(node: &serde_json::Value) -> Option<(String, f64)> {
        if let Some(id) = node.get("block_id").and_then(|b| b.as_str()) {
            let w = node.get("rect")?.as_array()?.get(2)?.as_f64()?;
            return Some((id.to_string(), w));
        }
        let split = node.get("split")?;
        first(split.get("before")?).or_else(|| first(split.get("after")?))
    }
    let (id, share) = first(layout)?;
    let cols = rex.terminal_size(&id).ok()?.columns;
    (share > 0.0 && cols > 0).then(|| (f64::from(cols) / share).round() as u16)
}

/// The block labelled `label` right beside `block` in a row of two.
fn block_beside(view: &serde_json::Value, block: &str, label: &str) -> Option<String> {
    fn walk(node: &serde_json::Value, block: &str) -> Option<String> {
        let split = node.get("split")?;
        let id = |n: &serde_json::Value| n.get("block_id").and_then(|i| i.as_str()).map(String::from);
        if split.get("direction").and_then(|d| d.as_str()) == Some("horizontal")
            && split.get("before").and_then(id).as_deref() == Some(block)
            && let Some(after) = split.get("after").and_then(id)
        {
            return Some(after);
        }
        walk(split.get("before")?, block).or_else(|| walk(split.get("after")?, block))
    }
    let layer = tiled_layer_of(view, block)?;
    let found = walk(layer.get("layout")?, block)?;
    let labelled = layer.get("blocks")?.as_array()?.iter().any(|b| {
        b.get("block_id").and_then(|i| i.as_str()) == Some(found.as_str()) && b.get("label").and_then(|l| l.as_str()) == Some(label)
    });
    labelled.then_some(found)
}

/// Show an agent's session pane right of `beside` when it's minimized
/// (detached), or minimize it again when it's showing.
fn toggle_agent_pane(rex: &axel_rex::Rex, block: &str, beside: &str) {
    let placed = rex
        .api_call("session.describe_block", serde_json::json!({ "block_id": block }))
        .ok()
        .and_then(|d| d.pointer("/block/placement").and_then(|p| p.as_str()).map(|p| p == "placed"))
        .unwrap_or(false);
    let _ = if placed {
        rex.api_call("session.detach_block", serde_json::json!({ "block_id": block }))
    } else {
        rex.api_call(
            "session.move_block",
            serde_json::json!({ "block_id": block, "anchor_block_id": beside, "direction": "horizontal", "side": "after", "ratio": 0.6 }),
        )
    };
}

/// Where a question floats: centered, a third of the window tall.
const QUESTION_BOUNDS: [f64; 4] = [0.2, 0.3, 0.6, 0.36];
/// The floating question pane's label.
const QUESTION_LABEL: &str = "question";

/// Open `axel thread answer` for `item_id` in a floating pane over this
/// window, focused, so you can answer it right away.
fn open_question(rex: &axel_rex::Rex, thread_id: &str, item_id: &str) {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "axel".into());
    let cmd = vec![exe, "thread".into(), "answer".into(), thread_id.to_string(), item_id.to_string()];
    if let Err(e) = rex.new_floating_terminal_with_focus(QUESTION_LABEL, QUESTION_BOUNDS, &cmd, None, &passthrough_env(), true) {
        eprintln!("axel: could not open the question: {e}");
    }
}

/// `axel thread answer`: one question, alone; the floating layer closes with it.
pub fn run_answer(thread_id: &str, item_id: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    let cwd = std::env::current_dir().context("reading current directory")?;
    let send = |session_id: &str, cmd: SendCommand| -> Result<SendReply> { send_to_session(&ctx, session_id, cmd) };
    let summon = |_: &str, _: &str, _: &str, _: &[std::path::PathBuf]| -> Result<String> { anyhow::bail!("not from here") };
    let runtime_label = |h: &axel_store::Harness| launch::runtime_label(h);
    let deps = Deps { store: &ctx.store, keystore: &ctx.keystore, cwd, harnesses: harnesses(), send: &send, summon: &summon, runtime_label: &runtime_label, open_thread: None, focus_peer: None, fit_prompt: None, manage_thread: None };
    let res = axel_tui::run_answer(deps, thread_id, item_id);
    close_own_layer(&ctx);
    res
}

/// Run `f` and exit when this process is hung up (SIGHUP).  The handler only
/// writes a byte to a pipe, whichever thread the signal lands on; a thread of
/// our own reads it and does the work.
fn on_hangup(f: impl FnOnce() + Send + 'static) {
    static WAKE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
    extern "C" fn wake(_: libc::c_int) {
        let fd = WAKE.load(std::sync::atomic::Ordering::Relaxed);
        if fd >= 0 {
            // SAFETY: write(2) is async-signal-safe; one byte from a static buffer.
            unsafe { libc::write(fd, b"h".as_ptr().cast(), 1) };
        }
    }
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe(2) fills two descriptors we own from here on.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return;
    }
    WAKE.store(fds[1], std::sync::atomic::Ordering::Relaxed);
    // SAFETY: installs a handler that only calls write(2).
    unsafe { libc::signal(libc::SIGHUP, wake as *const () as libc::sighandler_t) };
    let read_fd = fds[0];
    std::thread::spawn(move || {
        let mut byte = 0u8;
        // SAFETY: a blocking one-byte read from our own pipe.
        if unsafe { libc::read(read_fd, (&mut byte as *mut u8).cast(), 1) } == 1 {
            // The hangup reaches the `rex` calls `f` makes too: they inherit
            // an ignored SIGHUP and live to finish.
            // SAFETY: setting a signal's disposition to SIG_IGN.
            unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
            f();
            std::process::exit(129);
        }
    });
}

/// The stream pane paired with this composer: `$AXEL_STREAM_BLOCK` when the
/// composer was split off it, else the pane right above it, else the block
/// labelled `thread` in this window.
fn stream_block(rex: &axel_rex::Rex) -> Option<String> {
    if let Ok(b) = std::env::var("AXEL_STREAM_BLOCK")
        && !b.trim().is_empty()
    {
        return Some(b);
    }
    let me = rex.env().block.clone();
    // Docked or in its own tab, the stream is the pane right above.
    if let Some(above) = rex.view().ok().and_then(|v| block_above(&v, &me)) {
        return Some(above);
    }
    let d = rex.api_call("session.describe_block", serde_json::json!({ "block_id": me })).ok()?;
    let window = d.pointer("/block/window_id").and_then(|w| w.as_str())?.to_string();
    let list = rex.api_call("session.list_blocks", serde_json::json!({})).ok()?;
    list.get("blocks")?.as_array()?.iter().find_map(|b| {
        let id = b.get("block_id")?.as_str()?;
        (b.get("window_id").and_then(|w| w.as_str()) == Some(window.as_str())
            && b.get("label").and_then(|l| l.as_str()) == Some("thread")
            && id != me)
            .then(|| id.to_string())
    })
}

/// Move the border between the stream pane and this composer pane so the
/// composer gets `want` rows.  Rex sizes splits by ratio, and each pane's
/// title bar takes rows the terminal doesn't have, so the ratio comes from
/// both panes' rows and their rectangles in the window (see [`split_ratio_for`]).
fn fit_compose_pane(rex: &axel_rex::Rex, stream_id: &str, want: u16, now: u16) {
    let me = rex.env().block.clone();
    let Ok(sz) = rex.terminal_size(stream_id) else { return };
    // This pane's rows as Rex has them, read with the layout, so both come
    // from the same moment (the composer's own size can lag a resize).
    let now = rex.terminal_size(&me).map(|s| s.rows).unwrap_or(now);
    // Rex won't go below its least: asking for less only moves the border.
    let want = want.max(PANE_MIN_ROWS);
    if want == now {
        return;
    }
    let rects = rex.view().ok().and_then(|v| Some((block_rect(&v, stream_id)?, block_rect(&v, &me)?)));
    let ratio = match rects {
        Some((above, below)) => split_ratio_for((above[3], sz.rows), (below[3], now), want),
        None => None,
    }
    .unwrap_or_else(|| {
        // No geometry: rows alone, aiming a quarter row past `want`.
        let total = f64::from(sz.rows) + f64::from(now);
        ((total - f64::from(want) - 0.25) / total).clamp(0.2, 0.95)
    });
    let sent = rex.api_call(
        "session.resize_pane",
        serde_json::json!({ "direction": "vertical", "before_block_id": stream_id, "after_block_id": me, "ratio": ratio }),
    );
    // What each fit asked and got, to see why a pane lands off its size.
    let got = rex.terminal_size(&me).map(|s| s.rows).ok();
    fit_log(&format!(
        "fit want={want} now={now} stream_rows={} rects={rects:?} ratio={ratio:.4} sent={} rows_after={got:?}",
        sz.rows,
        if sent.is_ok() { "ok".to_string() } else { format!("{sent:?}") }
    ));
}

/// Rows Rex gives a pane at the least: the composer can't be shorter.
const PANE_MIN_ROWS: u16 = 5;

/// Put a new thread's borders where they belong right away, from sizes we
/// already know (the pane it replaces: `rows` tall, `width` wide), instead
/// of the half-and-half Rex's app first lays a new split out at.  The
/// composer then only fine-tunes.
fn settle_thread_row(rex: &axel_rex::Rex, stream: &str, compose: &str, usage: Option<&str>, rows: Option<u16>, width: Option<u16>) {
    if let (Some(usage), Some(keep)) = (usage, width.and_then(usage_keep)) {
        let _ = rex.api_call(
            "session.resize_pane",
            serde_json::json!({ "direction": "horizontal", "before_block_id": compose, "after_block_id": usage, "ratio": keep }),
        );
    }
    if let Some(rows) = rows.filter(|r| *r > PANE_MIN_ROWS * 2) {
        let ratio = ((f64::from(rows) - f64::from(PANE_MIN_ROWS) - 0.25) / f64::from(rows)).clamp(0.5, 0.95);
        let _ = rex.api_call(
            "session.resize_pane",
            serde_json::json!({ "direction": "vertical", "before_block_id": stream, "after_block_id": compose, "ratio": ratio }),
        );
    }
}

/// The tokens pane beside this composer at its width: Rex's app lays a new
/// row out on its own, so the border is put where it belongs once we're up
/// (and again on each fit).
fn fit_usage_pane(rex: &axel_rex::Rex, usage: &str, stream: &str) {
    let me = rex.env().block.clone();
    // 80×24 is a pane Rex hasn't laid out yet: its width says nothing.
    let size = rex.terminal_size(stream).ok().filter(|s| (s.columns, s.rows) != (80, 24));
    let Some(keep) = size.and_then(|s| usage_keep(s.columns)) else { return };
    let sent = rex.api_call(
        "session.resize_pane",
        serde_json::json!({ "direction": "horizontal", "before_block_id": me, "after_block_id": usage, "ratio": keep }),
    );
    fit_log(&format!("usage keep={keep:.4} sent={}", if sent.is_ok() { "ok".to_string() } else { format!("{sent:?}") }));
}

/// A line in `~/.axel/logs/compose-fit.log`.
fn fit_log(line: &str) {
    use std::io::Write;
    let path = crate::paths::home().join("logs").join("compose-fit.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {line}", chrono::Local::now().format("%H:%M:%S%.3f"));
    }
}

/// The split ratio (share of the pair's height for the pane above) that gives
/// the pane below `want` rows, from each pane's height as a fraction of the
/// window and its rows now.  Both panes lose the same rows to chrome, so two
/// panes give the window's rows per fraction (`k`) and that chrome (`c`):
/// `rows = k · height - c`.  `None` when the two can't tell them apart.
fn split_ratio_for(above: (f64, u16), below: (f64, u16), want: u16) -> Option<f64> {
    let ((ha, ra), (hb, rb)) = ((above.0, f64::from(above.1)), (below.0, f64::from(below.1)));
    if (ha - hb).abs() < 1e-6 || ha <= 0.0 || hb <= 0.0 {
        return None;
    }
    let k = (ra - rb) / (ha - hb);
    let c = k * hb - rb;
    // Readings taken mid-resize don't fit a window: a few rows of chrome per
    // pane, at least a screenful of rows. Fall back rather than trust them.
    if !(k.is_finite() && k >= 10.0 && (0.0..=8.0).contains(&c)) {
        return None;
    }
    // A quarter row past `want`: Rex rounding the split, down or to the
    // nearest row, lands on it. (Half a row rounded up to a spare row.)
    let below_h = (f64::from(want) + c + 0.25) / k;
    Some((1.0 - below_h / (ha + hb)).clamp(0.2, 0.95))
}

/// A block's rectangle `[x, y, w, h]` (fractions of its window), from `session.view`.
fn block_rect(view: &serde_json::Value, block: &str) -> Option<[f64; 4]> {
    fn walk(node: &serde_json::Value, block: &str) -> Option<[f64; 4]> {
        if node.get("block_id").and_then(|b| b.as_str()) == Some(block) {
            let r = node.get("rect")?.as_array()?;
            let n = |i: usize| r.get(i).and_then(|v| v.as_f64());
            return Some([n(0)?, n(1)?, n(2)?, n(3)?]);
        }
        let split = node.get("split")?;
        walk(split.get("before")?, block).or_else(|| walk(split.get("after")?, block))
    }
    walk(tiled_layer_of(view, block)?.get("layout")?, block)
}

/// Environment the floating pane should inherit from the invoking shell.
/// Panes Axel opens run with no login shell, so PATH must come from here or
/// adapters installed by nvm/homebrew are invisible to them.
fn passthrough_env() -> Vec<(String, String)> {
    ["PATH", "HOME", "AXEL_DB", "AXEL_HOME", "AXEL_HOOK_WAIT_SECS", "AXEL_KEYSTORE"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// Deliver a composer command to a managed session over its socket.
fn send_to_session(ctx: &Ctx, session_id: &str, cmd: SendCommand) -> Result<SendReply> {
    let session = ctx.store.get_session(session_id)?.with_context(|| format!("no session {session_id}"))?;
    if session.runtime != SessionRuntime::Managed {
        return Ok(SendReply::Error { message: "this agent runs in its own pane; press o to open it".into() });
    }
    let remote = super::host::host_of(ctx, &session)?.is_some();
    let sock = session.socket_path.clone();
    if !remote && sock.is_none() {
        return Ok(SendReply::Error { message: "session host not ready yet".into() });
    }
    let cmd = match cmd {
        SendCommand::Prompt { text, attachments, steer } => axel_acp::socket::Command::Prompt { text, attachments, steer, from: None },
        SendCommand::Steer { item_id } => axel_acp::socket::Command::Steer { item_id },
        SendCommand::Dequeue { item_id } => axel_acp::socket::Command::Dequeue { item_id },
        SendCommand::Edit { item_id, text } => axel_acp::socket::Command::Edit { item_id, text },
        SendCommand::SetMode { mode } => axel_acp::socket::Command::SetMode { mode },
        SendCommand::Cancel => axel_acp::socket::Command::Cancel,
        SendCommand::Answer { item_id, option_id } => axel_acp::socket::Command::Answer { item_id, option_id },
    };
    let reply = if remote {
        super::host::relay(ctx, &session, &cmd)
    } else {
        match axel_acp::socket::send(sock.as_deref().unwrap(), &cmd) {
            // Its host is gone (its pane closed, a crash): the session is over.
            // A message brings the agent back, resuming its conversation.
            Err(e) if host_gone(&e) => {
                return match cmd {
                    axel_acp::socket::Command::Prompt { text, attachments, .. } => revive(ctx, &session, &text, &attachments),
                    _ => {
                        end_dead_session(ctx, &session);
                        Ok(SendReply::Error { message: "its session had stopped; send a message to resume it".into() })
                    }
                };
            }
            r => r.map_err(anyhow::Error::from),
        }
    };
    Ok(match reply {
        Ok(axel_acp::socket::Reply::Ok) => SendReply::Ok,
        Ok(axel_acp::socket::Reply::Queued { item_id }) => SendReply::Queued { item_id },
        Ok(axel_acp::socket::Reply::Steering { item_id }) => SendReply::Steering { item_id },
        Ok(axel_acp::socket::Reply::Status { .. }) => SendReply::Ok,
        Ok(axel_acp::socket::Reply::Error { message }) => SendReply::Error { message },
        Err(e) => SendReply::Error { message: format!("host unreachable: {e}") },
    })
}

/// The socket's host process is gone: nothing listens (refused), or the
/// socket file is gone with it.
fn host_gone(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound)
}

/// Mark a session whose host has died as ended.
fn end_dead_session(ctx: &Ctx, session: &axel_store::Session) {
    if let Ok(owner) = ctx.owner() {
        let _ = ctx.store.end_session(&owner, &session.id, Some("its session host had stopped"));
    }
}

/// A message for a session whose host has died: end that session, and
/// resume the agent on the thread (its harness conversation, when it has
/// one) with the message as the first prompt.
fn revive(ctx: &Ctx, session: &axel_store::Session, text: &str, attachments: &[std::path::PathBuf]) -> Result<SendReply> {
    end_dead_session(ctx, session);
    let thread = ctx.store.get_thread(&session.thread_id)?.with_context(|| format!("no thread {}", session.thread_id))?;
    let req = axel_tui::LaunchRequest {
        title: thread.title.clone(),
        projects: vec![],
        agent_pubkey: session.agent_pubkey.clone(),
        workspace: axel_store::WorkspaceKind::InPlace,
        branch: None,
        thread_id: Some(thread.id),
        attachments: vec![],
    };
    let opts = launch::LaunchOptions {
        target: launch::Target::NewWindow,
        runtime: Some(SessionRuntime::Managed),
        prompt: Some(text.to_string()),
        attachments: attachments.to_vec(),
        work_dir: None,
        resume: session.harness_session_id.clone(),
    };
    match launch::launch(ctx, req, opts) {
        Ok(launch::Launched::Managed { host_spec: Some(spec), session_id, .. }) => launch::spawn_detached(&spec, &session_id)?,
        Ok(_) => {}
        Err(e) => return Ok(SendReply::Error { message: format!("its session had stopped, and resuming it failed: {e:#}") }),
    }
    Ok(SendReply::Ok)
}

/// Start another agent on a thread with `text` as its first message.
pub(crate) fn summon(ctx: &Ctx, thread_id: &str, agent_name: &str, text: &str, files: &[std::path::PathBuf]) -> Result<String> {
    // `atlas@mini` runs it on that host, `atlas@local` here; plain `atlas`
    // wherever it runs by default.
    let (name, asked) = match agent_name.split_once('@') {
        Some((a, h)) => (a, Some(h)),
        None => (agent_name, None),
    };
    let agent = ctx.store.agent_by_name(name)?.with_context(|| format!("no agent named {name:?}"))?;
    if let super::host::Placement::Host(h) = super::host::placement(ctx, &agent.pubkey, asked)? {
        return super::host::launch_remote(ctx, &h.label, thread_id, &agent.name, text, files);
    }
    let thread = ctx.store.get_thread(thread_id)?.with_context(|| format!("no thread {thread_id}"))?;
    // Back on a thread it worked on: it picks up its own conversation there.
    let resume = previous_conversation(ctx, &thread.id, &agent.pubkey);
    let req = axel_tui::LaunchRequest {
        title: thread.title.clone(),
        projects: vec![],
        agent_pubkey: agent.pubkey,
        workspace: axel_store::WorkspaceKind::InPlace,
        branch: None,
        thread_id: Some(thread.id),
        attachments: vec![],
    };
    let opts = launch::LaunchOptions { target: launch::Target::NewWindow, runtime: Some(SessionRuntime::Managed), prompt: Some(text.to_string()), attachments: files.to_vec(), work_dir: None, resume };
    match launch::launch(ctx, req, opts)? {
        // No Rex to host it in (e.g. summoned from an agent's process): run detached.
        launch::Launched::Managed { session_id, host_spec: Some(spec), .. } => {
            launch::spawn_detached(&spec, &session_id)?;
            Ok(session_id)
        }
        launch::Launched::Managed { session_id, .. } => Ok(session_id),
        launch::Launched::RexWindow { session_id, .. } => Ok(session_id),
        launch::Launched::Inline { session_id, .. } => Ok(session_id),
    }
}

/// The harness conversation of the agent's last session on the thread, to
/// resume when it comes back to the thread.
fn previous_conversation(ctx: &Ctx, thread_id: &str, agent: &str) -> Option<String> {
    ctx.store
        .list_sessions(axel_store::SessionFilter { thread_id: Some(thread_id.to_string()), agent_pubkey: Some(agent.to_string()), ..Default::default() })
        .ok()?
        .into_iter()
        .find_map(|s| s.harness_session_id)
}

/// Full id, unique id prefix, or case-insensitive title substring.
pub fn resolve_thread_id(ctx: &Ctx, id_or_prefix: &str) -> Result<String> {
    if let Some(t) = ctx.store.get_thread(id_or_prefix)? {
        return Ok(t.id);
    }
    let all = ctx.store.list_threads(axel_store::ThreadFilter::default())?;
    let hits: Vec<_> = all.into_iter().filter(|t| t.id.starts_with(id_or_prefix) || t.title.to_lowercase().contains(&id_or_prefix.to_lowercase())).collect();
    match hits.len() {
        0 => anyhow::bail!("no thread matches {id_or_prefix}"),
        1 => Ok(hits.into_iter().next().unwrap().id),
        n => anyhow::bail!("{n} threads match {id_or_prefix}; be more specific"),
    }
}

/// The last path component of `dir`: what a window opened there is called.
fn project_label(dir: &std::path::Path) -> Option<String> {
    dir.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty())
}

/// Rex's placeholder for a window nobody named: `Window` and the id's last 8 characters.
fn is_placeholder_window_label(window_id: &str, label: &str) -> bool {
    let tail: String = window_id.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    label.trim().is_empty() || label == format!("Window {tail}")
}

/// Name the window this pane is in, unless someone named it: only Rex's
/// placeholder, or one of the names in `ours` (names Axel gave it), is replaced.
fn name_own_window(rex: &axel_rex::Rex, name: &str, ours: &[&str]) {
    let Ok(Some(window)) = window_of_block(rex, &rex.env().block) else { return };
    let Ok(view) = rex.view() else { return };
    let label = view
        .get("windows")
        .and_then(|w| w.as_array())
        .into_iter()
        .flatten()
        .find(|w| w.get("window_id").and_then(|i| i.as_str()) == Some(window.as_str()))
        .and_then(|w| w.get("label").and_then(|l| l.as_str()))
        .unwrap_or("")
        .to_string();
    if label != name && (is_placeholder_window_label(&window, &label) || ours.contains(&label.as_str())) {
        rex.set_window_label(&window, name).ok();
    }
}

/// Once a thread runs in this window, the window takes its title (replacing
/// the project name the launcher gave it).
fn name_window_after_thread(ctx: &Ctx, rex: &axel_rex::Rex, thread_id: &str) {
    let Ok(Some(t)) = ctx.store.get_thread(thread_id) else { return };
    let title = axel_tui::util::ellipsize(&t.title, 40);
    let project = std::env::current_dir().ok().as_deref().and_then(project_label);
    name_own_window(rex, &title, project.as_deref().into_iter().collect::<Vec<_>>().as_slice());
}

/// A pane showing a thread view is labelled with the thread's title.
fn label_block_with_thread(ctx: &Ctx, rex: &axel_rex::Rex, block: &str, thread_id: &str) {
    if let Ok(Some(t)) = ctx.store.get_thread(thread_id) {
        let title: String = if t.title.chars().count() > 48 { format!("{}…", t.title.chars().take(47).collect::<String>()) } else { t.title.clone() };
        rex.set_block_label(block, &title).ok();
    }
}

#[cfg(test)]
mod dock_tests {
    use super::*;
    use serde_json::json;

    /// A window: the user's shell on the left, a docked thread on the right.
    fn view() -> serde_json::Value {
        json!({"windows": [
            {"window_id": "window:other", "layers": [{"kind": "tiled", "layout": {"block_id": "block:x"}, "blocks": [{"block_id": "block:x", "label": "compose"}]}]},
            {"window_id": "window:1", "layers": [
                {"kind": "tiled", "layer_id": "layer:t",
                 "layout": {"split": {"split_id": "split:root", "direction": "horizontal", "ratio": 0.4,
                    "before": {"block_id": "block:shell"},
                    "after": {"split": {"split_id": "split:dock", "direction": "vertical", "ratio": 0.85,
                        "before": {"block_id": "block:stream"}, "after": {"block_id": "block:compose"}}}}},
                 "blocks": [{"block_id": "block:shell", "label": "shell"}, {"block_id": "block:stream", "label": "Ship it"}, {"block_id": "block:compose", "label": "compose"}]},
                {"kind": "floating", "layer_id": "layer:f", "blocks": [{"block_id": "block:launcher", "label": "why not today?"}]}
            ]}
        ]})
    }

    #[test]
    fn the_dock_anchors_on_the_root_of_the_launchers_window() {
        assert_eq!(tiled_root(&view(), "block:launcher").as_deref(), Some("split:root"));
        assert_eq!(tiled_root(&view(), "block:x").as_deref(), Some("block:x"), "a window with one pane anchors on it");
        assert_eq!(tiled_root(&view(), "block:nowhere"), None);
    }

    #[test]
    fn the_split_ratio_accounts_for_each_panes_title_bar() {
        // A 40-row window, title bars of a row per pane: the conversation has
        // 0.8 of it (32 → 31 rows), the composer 0.2 (8 → 7 rows).
        let r = split_ratio_for((0.8, 31), (0.2, 7), 9).unwrap();
        // 9 rows + its title bar + a quarter row is 10.25 of 40.
        assert!((r - (1.0 - 10.25 / 40.0)).abs() < 1e-9, "{r}");
        // Shrinking back: 7 rows asks for the 0.2 it has, give or take the quarter row.
        let back = split_ratio_for((0.75, 29), (0.25, 9), 7).unwrap();
        assert!((back - (1.0 - 8.25 / 40.0)).abs() < 1e-9, "{back}");
        // Rounded to the nearest row, or down, that is exactly 9 rows, not a spare 10th.
        let rows = |ratio: f64| (1.0 - ratio) * 40.0 - 1.0;
        assert_eq!((rows(r).round(), rows(r).floor()), (9.0, 9.0));
        assert_eq!(split_ratio_for((0.5, 19), (0.5, 19), 5), None, "equal panes: can't tell rows from chrome");
        assert_eq!(split_ratio_for((0.85, 54), (0.15, 54), 4), None, "a reading from mid-resize: not trusted");
    }

    #[test]
    fn the_tokens_pane_beside_the_composer_is_34_columns() {
        let keep = usage_keep(160).unwrap();
        assert!((keep - 126.0 / 160.0).abs() < 1e-9, "{keep}");
        assert!((usage_keep(120).unwrap() - 86.0 / 120.0).abs() < 1e-9, "a 120-column row: 34 of it, not half");
        // Never more than 35% of the row.
        assert!((usage_keep(80).unwrap() - 0.65).abs() < 1e-9);
        assert_eq!(usage_keep(70), None, "a narrow composer keeps its width");
    }

    #[test]
    fn the_composer_finds_its_stream_and_tokens_pane_in_a_row() {
        let view = serde_json::json!({ "windows": [{ "layers": [{ "kind": "tiled",
            "blocks": [{ "block_id": "S", "label": "thread" }, { "block_id": "C", "label": "compose" }, { "block_id": "U", "label": "tokens" }],
            "layout": { "split": { "direction": "vertical", "before": { "block_id": "S" },
                "after": { "split": { "direction": "horizontal", "before": { "block_id": "C" }, "after": { "block_id": "U" } } } } } }] }] });
        assert_eq!(block_above(&view, "C").as_deref(), Some("S"));
        assert_eq!(block_beside(&view, "C", "tokens").as_deref(), Some("U"));
        assert_eq!(block_beside(&view, "C", "other"), None, "only the tokens pane");
    }

    #[test]
    fn the_usage_pane_sits_in_the_top_right_corner() {
        let [x, y, w, h] = corner_bounds(200, 50);
        assert!((w - 48.0 / 200.0).abs() < 1e-9 && (h - 10.0 / 50.0).abs() < 1e-9, "48×9 cells and a title bar");
        assert!((x + w - 0.99).abs() < 1e-9 && y < 0.05, "against the right edge, at the top");
        let [x, _, w, h] = corner_bounds(60, 12);
        assert!(w <= 0.6 && h <= 0.5 && x >= 0.0, "a small window keeps room for the rest: {w} {h}");
    }

    #[test]
    fn a_blocks_rect_comes_from_the_layout_tree() {
        let v = json!({"windows": [{"window_id": "w", "layers": [{"kind": "tiled",
            "layout": {"rect": [0, 0, 1, 1], "split": {"split_id": "s", "direction": "vertical", "ratio": 0.8,
                "before": {"rect": [0, 0, 1, 0.8], "block_id": "block:stream"},
                "after": {"rect": [0, 0.8, 1, 0.2], "block_id": "block:compose"}}},
            "blocks": [{"block_id": "block:stream"}, {"block_id": "block:compose"}]}]}]});
        assert_eq!(block_rect(&v, "block:compose"), Some([0.0, 0.8, 1.0, 0.2]));
        assert_eq!(block_rect(&v, "block:stream"), Some([0.0, 0.0, 1.0, 0.8]));
        assert_eq!(block_rect(&v, "block:nowhere"), None);
    }

    #[test]
    fn a_host_is_gone_when_nothing_listens_or_its_socket_is_gone() {
        use std::io::{Error, ErrorKind};
        assert!(host_gone(&Error::from(ErrorKind::ConnectionRefused)), "os error 61");
        assert!(host_gone(&Error::from(ErrorKind::NotFound)));
        assert!(!host_gone(&Error::from(ErrorKind::TimedOut)), "busy, not gone: don't start a second one");
        // A real socket path nobody serves.
        let path = std::env::temp_dir().join(format!("axel-gone-{}.sock", std::process::id()));
        let err = axel_acp::socket::send(&path, &axel_acp::socket::Command::Status).unwrap_err();
        assert!(host_gone(&err), "{err}");
    }

    #[test]
    fn only_a_shell_at_its_prompt_is_idle() {
        let fg = |argv0: &str, name: &str| json!({"foreground": {"argv0": argv0, "name": name}});
        assert!(is_idle_shell(&fg("-zsh", "zsh")), "a login shell at its prompt");
        assert!(is_idle_shell(&fg("/bin/bash", "bash")));
        assert!(!is_idle_shell(&fg("vim", "vim")), "something running");
        assert!(!is_idle_shell(&fg("claude", "claude")));
        assert!(!is_idle_shell(&json!({"foreground": null})));
    }

    #[test]
    fn a_docked_composer_finds_its_stream() {
        assert_eq!(block_above(&view(), "block:compose").as_deref(), Some("block:stream"));
        assert_eq!(block_above(&view(), "block:shell"), None, "side by side is not above");
    }
}

#[cfg(test)]
mod window_name_tests {
    use super::*;

    #[test]
    fn only_rex_placeholder_names_count_as_unnamed() {
        let id = "window:01a192a4-39aa-7971-9146-3a8b7ca9480b";
        assert!(is_placeholder_window_label(id, "Window 7ca9480b"));
        assert!(is_placeholder_window_label(id, ""));
        assert!(!is_placeholder_window_label(id, "Window 1b574bf3"), "another window's placeholder is a name");
        assert!(!is_placeholder_window_label(id, "api work"));
        assert_eq!(project_label(std::path::Path::new("/Users/ludo/Coding/axel")).as_deref(), Some("axel"));
    }
}
