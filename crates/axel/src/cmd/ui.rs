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
/// `usage`: a small square HUD, ~300×300 px, computed from the real cell size.
const USAGE_PX: f64 = 300.0;
const USAGE_FALLBACK: [f64; 4] = [0.76, 0.64, 0.22, 0.34];
/// The prompt pane's label, which Rex shows as its title.
pub const PROMPT_LABEL: &str = "why not today?";
pub const USAGE_LABEL: &str = "usage";
pub const THREADS_LABEL: &str = "threads";

/// Harness names the new-agent form cycles through: the builtin kinds, in order.
pub fn harnesses() -> Vec<&'static str> {
    axel_config::HarnessKind::BUILTIN.iter().map(|k| k.as_str()).collect()
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
    let env: Vec<(String, String)> = passthrough_env();

    // 1. `usage`: the agents HUD, behind the prompt, unfocused.
    let usage_bounds = usage_bounds(&rex).unwrap_or(USAGE_FALLBACK);
    let usage_cmd = vec![exe.clone(), "ui".into(), "--screen".into(), "dashboard".into()];
    if let Err(e) = rex.new_floating_terminal_with_focus(USAGE_LABEL, usage_bounds, &usage_cmd, cwd.as_deref(), &env, false) {
        eprintln!("axel: could not open the usage pane: {e}");
    }
    // 2. `prompt`: what are we building. Focused, in front.
    let prompt_cmd = vec![exe.clone(), "ui".into(), "--screen".into(), "launch".into()];
    let layer = rex
        .new_floating_terminal_with_focus(PROMPT_LABEL, PROMPT_BOUNDS, &prompt_cmd, cwd.as_deref(), &env, true)
        .context("opening the prompt pane in Rex")?;
    // 3. `threads`: recent threads, split in under the prompt, unfocused.
    if let Some(prompt) = layer.block_ids.first() {
        let threads_cmd = vec![exe, "ui".into(), "--screen".into(), "threads".into()];
        if let Err(e) = rex.split_below(prompt, THREADS_LABEL, &threads_cmd, PROMPT_RATIO, cwd.as_deref(), &env, false) {
            eprintln!("axel: could not open the threads pane: {e}");
        }
    }
    Ok(())
}

/// ~300×300 px as window fractions, from our own block's cell size. Anchored
/// to the bottom-right corner of the window with a small margin.
fn usage_bounds(rex: &axel_rex::Rex) -> Option<[f64; 4]> {
    let size = rex.terminal_size(&rex.env().block).ok()?;
    let (cw, ch) = (size.cell_width_px? as f64, size.cell_height_px? as f64);
    if size.columns == 0 || size.rows == 0 {
        return None;
    }
    // Cell sizes come in device pixels; on a Retina display (tall cells) the
    // 300 px the user sees is 600 device pixels.
    let scale = if ch > 20.0 { 2.0 } else { 1.0 };
    let target = USAGE_PX * scale;
    // Our block may be a split, not the whole window; the window is at least this big.
    let win_w = size.columns as f64 * cw;
    let win_h = size.rows as f64 * ch;
    let w = (target / win_w).clamp(0.14, 0.60);
    let h = (target / win_h).clamp(0.14, 0.70);
    const MARGIN: f64 = 0.02;
    let x = (1.0 - w - MARGIN).max(0.0);
    let y = (1.0 - h - MARGIN).max(0.0);
    Some([x, y, w, h])
}

/// Floating layers in the current window whose first block is one of ours.
fn axel_layers(rex: &axel_rex::Rex) -> Result<Vec<String>> {
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
            if (label == PROMPT_LABEL || label == "prompt" || label == USAGE_LABEL || label == "axel")
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
    // Inside Rex a thread opens as its own tab: the conversation streaming
    // natively above, the composer below.  The dashboard stays where it is
    // (a floating dashboard closes, as when focusing a session).
    let open_thread = |tid: &str| -> Result<()> {
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
    let fit_prompt = |expanded: bool| {
        if let (Some(rex), Some((me, threads, true))) = (&ctx.rex, &peer) {
            let ratio = if expanded { PROMPT_RATIO_EXPANDED } else { PROMPT_RATIO };
            let _ = rex.api_call(
                "session.resize_pane",
                serde_json::json!({ "direction": "vertical", "before_block_id": me, "after_block_id": threads, "ratio": ratio }),
            );
        }
    };
    let manage_thread = |tid: &str, op: axel_tui::ThreadOp| -> Result<()> { super::thread::set_state(&ctx, tid, op) };
    let focus_peer_ref: Option<&dyn Fn()> = if peer.is_some() { Some(&focus_peer) } else { None };
    let fit_prompt_ref: Option<&dyn Fn(bool)> = if matches!(peer, Some((_, _, true))) { Some(&fit_prompt) } else { None };
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

/// The launcher pane becomes `thread_id`'s view: inside Rex, the stream
/// with the composer split in below; elsewhere, the thread screen.
fn open_thread_here(ctx: &Ctx, thread_id: &str) -> Result<()> {
    if let Some(rex) = &ctx.rex {
        let block = rex.env().block.clone();
        if let Ok(Some(layer)) = layer_of_block(rex, &block) {
            rex.resize_layer(&layer, DASHBOARD_BOUNDS).ok();
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
fn close_own_layer(ctx: &Ctx) {
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
    rex.split_below(&me, "compose", &compose, 0.85, None, &env, true)?;
    let owner_pk = ctx.owner()?.public().to_string();
    axel_tui::run_stream(&ctx.store, &owner_pk, thread_id)
}

/// A new Rex tab with the stream above and the composer below; the composer gets focus.
fn open_split_tab(ctx: &Ctx, thread_id: &str) -> Result<()> {
    let rex = ctx.rex.as_ref().context("not inside Rex")?;
    let thread = ctx.store.get_thread(thread_id)?.with_context(|| format!("no thread {thread_id}"))?;
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "axel".into());
    let stream: Vec<String> = vec![exe.clone(), "thread".into(), "stream".into(), thread.id.clone()];
    let compose: Vec<String> = vec![exe, "thread".into(), "compose".into(), thread.id.clone()];
    let label = axel_tui::util::ellipsize(&thread.title, 40);
    let w = rex.new_window_stacked(&label, ("thread", &stream), ("compose", &compose), 0.8, None, &passthrough_env(), true)?;
    if let Some(b) = w.block_ids.get(1) {
        let _ = rex.api_call("session.focus_block", serde_json::json!({ "block_id": b }));
    }
    Ok(())
}

/// `axel thread stream`: the top pane.
pub fn run_stream(thread_id: &str) -> Result<()> {
    let ctx = Ctx::open()?;
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
    let fit = |want: u16, now: u16| {
        if let (Some(rex), Some(stream)) = (&ctx.rex, &stream) {
            fit_compose_pane(rex, stream, want, now);
        }
    };
    let res = axel_tui::run_compose(deps, thread_id, &fit);
    // Leaving closes the stream above; this pane closes as its program exits.
    // Only those two blocks: the window may hold the user's other panes.
    if let (Some(rex), Some(stream)) = (&ctx.rex, &stream) {
        let _ = rex.api_call("block.close", serde_json::json!({ "block_id": stream }));
    }
    res
}

/// The stream pane paired with this composer: `$AXEL_STREAM_BLOCK` when the
/// composer was split off it, else the block labelled `thread` in this window.
fn stream_block(rex: &axel_rex::Rex) -> Option<String> {
    if let Ok(b) = std::env::var("AXEL_STREAM_BLOCK")
        && !b.trim().is_empty()
    {
        return Some(b);
    }
    let me = rex.env().block.clone();
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
/// composer gets `want` rows.  Rex sizes splits by ratio, so the ratio comes
/// from both panes' current rows.
fn fit_compose_pane(rex: &axel_rex::Rex, stream_id: &str, want: u16, now: u16) {
    let me = rex.env().block.clone();
    let Ok(sz) = rex.terminal_size(stream_id) else { return };
    let total = f64::from(sz.rows) + f64::from(now);
    if total < 4.0 || want == now {
        return;
    }
    // Aim half a row past `want`, so rounding the split lands on it, not a row short.
    let ratio = ((total - f64::from(want) - 0.5) / total).clamp(0.2, 0.95);
    let _ = rex.api_call(
        "session.resize_pane",
        serde_json::json!({ "direction": "vertical", "before_block_id": stream_id, "after_block_id": me, "ratio": ratio }),
    );
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
        SendCommand::SetMode { mode } => axel_acp::socket::Command::SetMode { mode },
        SendCommand::Cancel => axel_acp::socket::Command::Cancel,
        SendCommand::Answer { item_id, option_id } => axel_acp::socket::Command::Answer { item_id, option_id },
    };
    let reply = if remote {
        super::host::relay(ctx, &session, &cmd)
    } else {
        axel_acp::socket::send(sock.as_deref().unwrap(), &cmd).map_err(anyhow::Error::from)
    };
    Ok(match reply {
        Ok(axel_acp::socket::Reply::Ok) => SendReply::Ok,
        Ok(axel_acp::socket::Reply::Queued { item_id }) => SendReply::Queued { item_id },
        Ok(axel_acp::socket::Reply::Status { .. }) => SendReply::Ok,
        Ok(axel_acp::socket::Reply::Error { message }) => SendReply::Error { message },
        Err(e) => SendReply::Error { message: format!("host unreachable: {e}") },
    })
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
    let req = axel_tui::LaunchRequest {
        title: thread.title.clone(),
        projects: vec![],
        agent_pubkey: agent.pubkey,
        workspace: axel_store::WorkspaceKind::InPlace,
        branch: None,
        thread_id: Some(thread.id),
        attachments: vec![],
    };
    let opts = launch::LaunchOptions { target: launch::Target::NewWindow, runtime: Some(SessionRuntime::Managed), prompt: Some(text.to_string()), attachments: files.to_vec(), work_dir: None, resume: None };
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
