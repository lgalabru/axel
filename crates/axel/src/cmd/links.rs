//! `axel-links://` links: a click on a run of tool calls in the conversation pane
//! opens its details in a floating Rex pane.
//!
//! macOS hands a URL to an app, not a command, so Axel keeps a small handler
//! app (`~/.axel/apps/Axel Links.app`): an AppleScript applet that declares
//! the `axel` scheme and passes each URL to `axel open-url`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use axel_tui::util::{CopyLink, ToolsLink};

use crate::paths;

mod drivers;

const APP_NAME: &str = "Axel Links.app";
const BUNDLE_ID: &str = "md.axel.links";
/// Which `axel` the app runs, so a moved binary means a rebuilt app.
const MARKER: &str = "Contents/Resources/axel-exe";
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

fn app_path() -> PathBuf {
    paths::home().join("apps").join(APP_NAME)
}

/// The AppleScript the applet runs: each URL to `axel open-url`, in the
/// background, its output appended to `log` (a click that does nothing says why).
fn applet_source(exe: &Path, log: &Path) -> String {
    let q = |p: &Path| p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let (exe, log) = (q(exe), q(log));
    format!(
        "on open location theURL\n\
         \tdo shell script quoted form of \"{exe}\" & \" open-url \" & quoted form of theURL & \" >> \" & quoted form of \"{log}\" & \" 2>&1 &\"\n\
         end open location\n"
    )
}

/// Where the handler app's runs of `axel open-url` write.
fn log_path() -> PathBuf {
    paths::home().join("logs").join("links.log")
}

/// The handler app is there, and runs this `axel`.
pub fn installed() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    std::fs::read_to_string(app_path().join(MARKER)).is_ok_and(|m| m.trim() == exe.to_string_lossy())
}

/// `axel links install`: build the handler app and register it for `axel-links://`.
pub fn install() -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!("`axel-links://` links need macOS");
    }
    let exe = std::env::current_exe().context("locating the axel binary")?;
    let app = app_path();
    if app.exists() {
        std::fs::remove_dir_all(&app).with_context(|| format!("replacing {}", app.display()))?;
    }
    std::fs::create_dir_all(app.parent().unwrap())?;
    std::fs::create_dir_all(log_path().parent().unwrap())?;
    run(Command::new("osacompile").arg("-o").arg(&app).arg("-e").arg(applet_source(&exe, &log_path())), "osacompile")?;
    let plist = app.join("Contents/Info.plist");
    let pb = |cmd: &str| run(Command::new("/usr/libexec/PlistBuddy").arg("-c").arg(cmd).arg(&plist), "PlistBuddy");
    // osacompile's applets have no identifier of their own.
    let _ = pb("Delete :CFBundleIdentifier");
    pb(&format!("Add :CFBundleIdentifier string {BUNDLE_ID}"))?;
    let _ = pb("Delete :CFBundleURLTypes");
    pb("Add :CFBundleURLTypes array")?;
    pb("Add :CFBundleURLTypes:0 dict")?;
    pb(&format!("Add :CFBundleURLTypes:0:CFBundleURLName string {BUNDLE_ID}"))?;
    pb("Add :CFBundleURLTypes:0:CFBundleURLSchemes array")?;
    pb(&format!("Add :CFBundleURLTypes:0:CFBundleURLSchemes:0 string {}", ToolsLink::SCHEME))?;
    // No Dock icon, no menu bar: it only hands URLs on.
    let _ = pb("Delete :LSUIElement");
    pb("Add :LSUIElement bool true")?;
    std::fs::write(app.join(MARKER), exe.to_string_lossy().as_bytes())?;
    run(Command::new("codesign").args(["--force", "--sign", "-"]).arg(&app), "codesign")?;
    run(Command::new(LSREGISTER).arg("-f").arg(&app), "lsregister")?;
    Ok(app)
}

/// Install the handler app in the background if it's missing or stale.
pub fn ensure_installed() {
    if cfg!(target_os = "macos") && !installed() {
        std::thread::spawn(|| {
            let _ = install();
        });
    }
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd.output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// `axel open-url URL`: what the handler app runs for each click.
pub fn open_url(url: &str) -> Result<()> {
    eprintln!("{} open-url {url}", chrono::Local::now().format("%H:%M:%S"));
    if let Some(copy) = CopyLink::parse(url) {
        return copy_command(&copy.item);
    }
    let link = ToolsLink::parse(url).with_context(|| format!("not an Axel link: {url}"))?;
    let rex = axel_rex::Rex::new(axel_rex::RexEnv { server: link.server.clone(), session: link.session.clone(), block: String::new() });
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let cmd = vec![exe, "thread".into(), "tools".into(), link.thread.clone(), link.fold.clone()];
    let label = super::Ctx::open().ok().and_then(|ctx| pane_label(&ctx, &link)).unwrap_or_else(|| TOOLS_LABEL.to_string());
    rex.new_floating_terminal_with_focus(&label, TOOLS_BOUNDS, &cmd, None, &[], true).context("opening the tool calls in Rex")?;
    Ok(())
}

/// Put the command a tool call ran on the clipboard, exactly as it ran.
fn copy_command(item: &str) -> Result<()> {
    let ctx = super::Ctx::open()?;
    let it = ctx.store.item(item)?.with_context(|| format!("no tool call {item}"))?;
    let cmd = command_of(&it).with_context(|| format!("tool call {item} ran no command"))?;
    use std::io::Write;
    // By its path: the handler app runs with a minimal PATH.
    let mut pb = Command::new("/usr/bin/pbcopy").stdin(std::process::Stdio::piped()).spawn().context("starting pbcopy")?;
    pb.stdin.take().context("pbcopy's input")?.write_all(cmd.as_bytes())?;
    if !pb.wait()?.success() {
        bail!("pbcopy failed");
    }
    eprintln!("copied {} bytes from {item}", cmd.len());
    Ok(())
}

/// The command a tool call ran, if it ran one.
fn command_of(it: &axel_store::ConversationItem) -> Option<String> {
    it.content.get("raw_input")?.get("command")?.as_str().map(str::to_string)
}

/// The pane's title: who ran the calls, and how many (`lyra · 3 tool calls`).
fn pane_label(ctx: &super::Ctx, link: &ToolsLink) -> Option<String> {
    let items = ctx.store.thread_items(&link.thread, 5_000).ok()?;
    let calls = fold_calls(&items, &link.fold);
    let who = calls.first().and_then(|c| agent_name(ctx, &c.session_id))?;
    Some(format!("{who} · {}", count(calls.len())))
}

fn count(n: usize) -> String {
    format!("{n} tool call{}", if n == 1 { "" } else { "s" })
}

fn agent_name(ctx: &super::Ctx, session_id: &str) -> Option<String> {
    let s = ctx.store.get_session(session_id).ok()??;
    ctx.store.agent_by_pubkey(&s.agent_pubkey).ok()?.map(|a| a.name)
}

/// `axel thread tools THREAD FOLD`: the run of tool calls a fold line stands
/// for, in full, as plain terminal output (scroll it natively); then a key
/// closes the floating pane.
pub fn show_tools(thread_id: &str, fold: &str) -> Result<()> {
    let ctx = super::Ctx::open()?;
    let thread = ctx.store.get_thread(thread_id)?.with_context(|| format!("no thread {thread_id}"))?;
    let items = ctx.store.thread_items(&thread.id, 5_000)?;
    let calls = fold_calls(&items, fold);
    let linker = Linker::new(thread_dir(&ctx, &thread.id), Editor::from_env());
    // How long each took: until the next thing on the thread.
    let start = items.iter().position(|i| i.id == fold).unwrap_or(0);
    let took = |k: usize| items.get(start + k + 1).map(|n| n.at - items[start + k].at);
    let who = calls.first().and_then(|c| agent_name(&ctx, &c.session_id)).unwrap_or_else(|| "agent".into());
    let span = match (calls.first(), took(calls.len().saturating_sub(1))) {
        (Some(f), Some(last)) => {
            let end = calls.last().map_or(f.at, |l| l.at) + last;
            format!("{}–{} · {}", clock(f.at), clock(end), duration(end - f.at))
        }
        (Some(f), None) => clock(f.at),
        _ => String::new(),
    };
    // Everything, wrapped to `width`, from the top of a cleared screen and
    // scrollback (the login banner goes, and so does an older width's copy).
    let render = |width: usize| {
        let mut out = String::from("\x1b[H\x1b[2J\x1b[3J");
        out.push_str(&format!("\x1b[1m{who}\x1b[0m · {}\x1b[2m · {span}\x1b[0m\r\n", count(calls.len())));
        out.push_str(&wrap_plain(&thread.title, width, "", DIM));
        for (k, it) in calls.iter().enumerate() {
            out.push_str("\r\n");
            out.push_str(&describe_call(it, took(k), width, &linker));
        }
        out.push_str("\r\n");
        out.push_str("\x1b[2mEsc or q closes · scroll as usual\x1b[0m\r\n");
        out
    };
    // The pane opens at a default size and is then laid out: print at the
    // width it settles on, and again whenever it's resized.
    let mut width = settled_width();
    use std::io::Write;
    print!("{}", render(width));
    let _ = std::io::stdout().flush();
    while let Some(w) = wait_for_close_or_resize() {
        if w != width {
            width = w;
            print!("{}", render(width));
            let _ = std::io::stdout().flush();
        }
    }
    super::ui::close_own_layer(&ctx);
    Ok(())
}

/// The fold starting at `first`: it and the tool calls right after it, in
/// the thread's order (as the conversation pane folds them).
fn fold_calls<'a>(items: &'a [axel_store::ConversationItem], first: &str) -> Vec<&'a axel_store::ConversationItem> {
    let Some(start) = items.iter().position(|i| i.id == first) else { return Vec::new() };
    items[start..].iter().take_while(|i| i.kind == axel_store::ItemKind::ToolCall).collect()
}

/// `18:09:49`, local time, from Unix milliseconds.
fn clock(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).unwrap_or_default()
}

/// `0.4s`, `12s`, `1m21s`.
fn duration(ms: i64) -> String {
    let ms = ms.max(0);
    match ms {
        0..=9_999 => format!("{:.1}s", ms as f64 / 1000.0),
        10_000..=59_999 => format!("{}s", ms / 1000),
        _ => format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000),
    }
}

/// What a click on a file opens: `$EDITOR`, when it is an editor with a URL
/// scheme that takes a line, else the file as `file://` (its default app).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Editor {
    File,
    Zed,
    VsCode,
    Cursor,
}

impl Editor {
    fn from_env() -> Self {
        Self::named(&std::env::var("EDITOR").unwrap_or_default())
    }

    /// From an `$EDITOR` value (`zed --wait`, `/usr/local/bin/code -w`).
    fn named(editor: &str) -> Self {
        let exe = editor.split_whitespace().next().unwrap_or("");
        match exe.rsplit('/').next().unwrap_or("") {
            "zed" | "zeditor" => Editor::Zed,
            "code" | "code-insiders" => Editor::VsCode,
            "cursor" => Editor::Cursor,
            _ => Editor::File,
        }
    }

    /// The link to `path` (absolute), at `line` when the editor can go there.
    fn url(self, path: &Path, line: Option<usize>) -> String {
        let path = url_path(path);
        let at = line.map(|l| format!(":{l}")).unwrap_or_default();
        match self {
            Editor::File => format!("file://{path}"),
            Editor::Zed => format!("zed://file{path}{at}"),
            Editor::VsCode => format!("vscode://file{path}{at}"),
            Editor::Cursor => format!("cursor://file{path}{at}"),
        }
    }
}

/// A path as a URL's path: `/` kept, anything but unreserved characters escaped.
fn url_path(path: &Path) -> String {
    let mut out = String::new();
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Links to the files a call names, for Cmd+click: paths relative to `base`
/// (where the thread works), and only to files that are there.
#[derive(Clone, Debug)]
struct Linker {
    base: Option<PathBuf>,
    editor: Editor,
}

impl Linker {
    fn new(base: Option<PathBuf>, editor: Editor) -> Self {
        Self { base, editor }
    }

    /// For what a command prints: relative to the directory it `cd`s into first.
    fn after_cd(&self, cmd: &str) -> Linker {
        let Some(rest) = cmd.trim_start().strip_prefix("cd ") else { return self.clone() };
        let end = rest.find("&&").or_else(|| rest.find(';')).unwrap_or(rest.len());
        let dir = rest[..end].trim();
        if dir.is_empty() || dir.contains(['"', '\'', '$', '`']) {
            return self.clone();
        }
        Linker { base: self.resolve(dir), editor: self.editor }
    }

    fn resolve(&self, path: &str) -> Option<PathBuf> {
        if let Some(rest) = path.strip_prefix("~/") {
            return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest));
        }
        let p = Path::new(path);
        if p.is_absolute() { Some(p.to_path_buf()) } else { self.base.as_ref().map(|b| b.join(p)) }
    }

    /// The link to `path` (at `line`), when it is a file.
    fn url(&self, path: &str, line: Option<usize>) -> Option<String> {
        let full = self.resolve(path)?;
        full.is_file().then(|| self.editor.url(&full, line))
    }
}

/// Where the thread works: its worktree, else its project's directory here.
fn thread_dir(ctx: &super::Ctx, thread_id: &str) -> Option<PathBuf> {
    let project = ctx.store.thread_projects(thread_id).ok()?.into_iter().next()?;
    if let Some(w) = project.workspace.worktree_path.filter(|p| p.is_dir()) {
        return Some(w);
    }
    let local = project.local_project_id.as_deref().and_then(|id| ctx.store.get_project(id).ok().flatten()).map(|p| p.path);
    local.or(Some(project.path_hint)).filter(|p| p.is_dir())
}

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
/// The most lines of a file's content or an output shown before "… N more".
const MAX_LINES: usize = 200;

/// The pane's width once it has held for a few reads (it opens at a default
/// size and is laid out right after).
fn settled_width() -> usize {
    let read = || crossterm::terminal::size().map(|(w, _)| w).unwrap_or(100);
    let (mut last, mut same) = (read(), 0);
    for _ in 0..15 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let now = read();
        if now == last {
            same += 1;
            if same >= 2 {
                break;
            }
        } else {
            (last, same) = (now, 0);
        }
    }
    usize::from(last).max(20)
}

/// The left margin of a call's details.
const INDENT: &str = "";

/// One call: what it was and how it went, then what it did, as its kind of
/// tool reads best (a command, a diff, a file and its lines, a search),
/// highlighted by language, then its output; wrapped to `width`.
fn describe_call(it: &axel_store::ConversationItem, took: Option<i64>, width: usize, links: &Linker) -> String {
    let c = &it.content;
    let input = c.get("raw_input").cloned().unwrap_or(serde_json::Value::Null);
    let field = |k: &str| input.get(k).and_then(|v| v.as_str());
    let kind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    // Claude's titles quote code in backticks (``Edit `path` ``): plain text here.
    // A command's title is the command itself: say what it's for instead, and
    // show the command once, below.
    let raw_title = c.get("title").and_then(|v| v.as_str()).unwrap_or("tool").replace('`', "");
    let title = match field("command") {
        Some(cmd) => field("description").map(str::to_string).unwrap_or_else(|| drivers::title_of(cmd)),
        None => raw_title.trim().to_string(),
    };
    let status = it.status.map(|s| s.as_str().to_string()).unwrap_or_default();
    let status_col = match status.as_str() {
        "completed" | "allowed" => GREEN,
        "failed" | "denied" | "cancelled" => RED,
        "running" | "pending" | "in_progress" => YELLOW,
        _ => DIM,
    };
    // A tick says it went fine; anything else says what happened.
    let status_label = match status.as_str() {
        "completed" => "✓".to_string(),
        "failed" => "✗ failed".to_string(),
        s => s.replace('_', " "),
    };
    let output = without_harness_notes(&output_text(c.get("raw_output")));
    let tool = field("command").map(drivers::tool_of);
    let mut meta = vec![clock(it.at)];
    if let Some(t) = took {
        meta.push(duration(t));
    }
    // The block below says it's a command.
    if !kind.is_empty() && tool.is_none() {
        meta.push(kind.to_string());
    }
    // `completed · 2 errors · 1 warning`: how it went, then what the tool said.
    let mut tail = vec![(status_label, status_col)];
    if let Some(s) = tool.and_then(|t| drivers::summary(t, &output)) {
        let bad = s.contains("error") || s.contains("failed");
        tail.push((s, if bad { RED } else { "" }));
    }
    let tail: Vec<(String, &str)> = tail.into_iter().filter(|(t, _)| !t.is_empty()).collect();
    // The box's sections: what was called, what it did, what came of it.
    let inner = width.saturating_sub(2 * BAND_PAD).max(16);
    let copy = field("command").map(|_| CopyLink { item: it.id.clone() }.url());
    let head = header(&title, &meta.join(" · "), &tail, copy.as_deref(), inner);
    let width = inner;
    let mut out = String::new();
    let path = field("file_path");
    if let Some(cmd) = field("command") {
        out.push_str(&command_block(cmd, inner));
    } else if let (Some(path), Some(old), Some(new)) = (path, field("old_string"), field("new_string")) {
        out.push_str(&wrap_linked(path, links.url(path, None).as_deref(), width, INDENT, BOLD));
        out.push_str(&diff_view(old, new, Lang::File(path), inner));
    } else if let (Some(path), Some(content)) = (path, field("content")) {
        out.push_str(&wrap_linked(&format!("{path}  ({} lines)", content.lines().count()), links.url(path, None).as_deref(), width, INDENT, BOLD));
        out.push_str(&highlighted(&first_lines(content), Lang::File(path), inner, ""));
    } else if let Some(path) = path {
        let range = match (input.get("offset").and_then(|v| v.as_i64()), input.get("limit").and_then(|v| v.as_i64())) {
            (Some(o), Some(l)) => format!("  (lines {o}–{})", o + l),
            (Some(o), None) => format!("  (from line {o})"),
            (None, Some(l)) => format!("  (first {l} lines)"),
            _ => String::new(),
        };
        let first = input.get("offset").and_then(|v| v.as_u64()).map(|o| o.max(1) as usize);
        out.push_str(&wrap_linked(&format!("{path}{range}"), links.url(path, first).as_deref(), width, INDENT, BOLD));
    } else if let Some(pattern) = field("pattern") {
        let place = [field("path"), field("glob")].into_iter().flatten().collect::<Vec<_>>().join("  ");
        out.push_str(&wrap_plain(&format!("/{pattern}/  {place}"), width, INDENT, ""));
    } else if let Some(url) = field("url") {
        out.push_str(&wrap_plain(url, width, INDENT, ""));
    } else if let Some(d) = field("description") {
        out.push_str(&wrap_plain(d, width, INDENT, ""));
        if let Some(prompt) = field("prompt") {
            out.push_str(&wrap_plain(&first_lines(prompt), width, INDENT, DIM));
        }
    } else if !input.is_null() {
        out.push_str(&highlighted(&serde_json::to_string_pretty(&input).unwrap_or_default(), Lang::Named("json"), inner, ""));
    }
    let paths: Vec<&str> = c.get("locations").and_then(|l| l.as_array()).into_iter().flatten().filter_map(|l| l.get("path").and_then(|p| p.as_str())).collect();
    if !paths.is_empty() && path.is_none() && field("command").is_none() {
        for p in paths {
            out.push_str(&wrap_linked(p, links.url(p, None).as_deref(), width, INDENT, DIM));
        }
    }
    let body = std::mem::take(&mut out);
    let output = output.replace('\t', "    ");
    if !output.trim().is_empty() {
        // What a read returns is the file, and so is the output of a command
        // that only prints one file (`sed -n 15,45p src/help.rs`): highlight it
        // as that file. Anything else stays plain.
        match (kind, path, strip_line_numbers(&output), field("command").and_then(file_printed_by)) {
            ("read", Some(p), Some(code), _) => out.push_str(&highlighted(&first_lines(&code), Lang::File(p), inner, "")),
            (_, _, _, Some(p)) => out.push_str(&highlighted(&first_lines(output.trim_end()), Lang::File(p), inner, "")),
            _ => match field("command").zip(tool).and_then(|(cmd, t)| drivers::tool_view(t, cmd, output.trim_end(), inner, links)) {
                Some(view) => out.push_str(&view),
                None => out.push_str(&wrap_plain(&first_lines(output.trim_end()), width, INDENT, DIM)),
            },
        }
    }
    let mut sections = vec![Section { text: head, band: Band::Header }];
    if !body.is_empty() {
        sections.push(Section { text: body, band: Band::Body });
    }
    if !out.is_empty() {
        sections.push(Section { text: out, band: Band::Result });
    }
    frame(&sections, inner + 2 * BAND_PAD)
}

/// A command's output without what the harness added after it (Claude Code's
/// `Shell cwd was reset to …`): only what the command printed.
fn without_harness_notes(output: &str) -> String {
    let mut lines: Vec<&str> = output.trim_end().lines().collect();
    while lines.last().is_some_and(|l| l.starts_with("Shell cwd was reset to ")) {
        lines.pop();
    }
    lines.join("\n")
}

/// The header's copy link, and how it looks.
const COPY_LABEL: &str = "⧉ copy";
const COPY_STYLE: &str = "\x1b[34m";

/// Which part of a call a band is: each its own shade of slate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Band {
    /// What was called, and how it went.
    Header,
    /// What it did: the command, the diff, the file.
    Body,
    /// What came of it: the output.
    Result,
}

impl Band {
    /// Its background, as SGR: slate, lightest for the header, darkest
    /// (nearly the black a command used to sit on) for the body.
    fn bg(self) -> &'static str {
        match self {
            Band::Header => "48;2;30;41;59",
            Band::Body => "48;2;2;6;23",
            Band::Result => "48;2;17;24;39",
        }
    }
}

/// One band of a call: its rendered lines.
struct Section {
    text: String,
    band: Band,
}

/// Columns of a band's shade either side of its text.
const BAND_PAD: usize = 2;

/// A call as bands the width of the pane, no border: each band's shade
/// behind it edge to edge, a line of it above and below its text, and the
/// bands touching. Every reset in a line puts the band's shade back, so
/// colours inside it (a diff's green and red) sit on it.
fn frame(sections: &[Section], width: usize) -> String {
    let room = width.saturating_sub(2 * BAND_PAD);
    let pad = " ".repeat(BAND_PAD);
    let mut out = String::new();
    for section in sections {
        let bg = format!("\x1b[{}m", section.band.bg());
        let blank = format!("{bg}{}{RESET}\r\n", " ".repeat(width));
        out.push_str(&blank);
        for line in section.text.trim_end_matches(['\r', '\n']).split("\r\n") {
            let (line, used) = fit(line, room);
            let line = line.replace(RESET, &format!("{RESET}{bg}"));
            out.push_str(&format!("{bg}{pad}{line}{RESET}{bg}{}{pad}{RESET}\r\n", " ".repeat(room - used)));
        }
        out.push_str(&blank);
    }
    out
}

/// A rendered line and how many columns it takes, cut to `room` (with `…`)
/// if it would push the border out. Escape sequences take none.
fn fit(line: &str, room: usize) -> (String, usize) {
    let line = line.replace('\t', "    ");
    let (whole, used) = cut(&line, usize::MAX);
    if used <= room {
        return (whole, used);
    }
    // One column for the ellipsis.
    let (mut out, used) = cut(&line, room.saturating_sub(1));
    let open = out.rfind("\x1b]8;;").is_some_and(|at| !out[at..].starts_with("\x1b]8;;\x1b"));
    out.push('…');
    if open {
        out.push_str("\x1b]8;;\x1b\\");
    }
    (out, used + 1)
}

/// `line` up to `room` columns, escape sequences kept whole.
fn cut(line: &str, room: usize) -> (String, usize) {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            match chars.peek() {
                // OSC (a hyperlink) up to its `ESC \\`.
                Some(']') => {
                    while let Some(d) = chars.next() {
                        out.push(d);
                        if d == '\x1b' && chars.peek() == Some(&'\\') {
                            out.push(chars.next().unwrap_or('\\'));
                            break;
                        }
                    }
                }
                // CSI up to its final letter.
                _ => {
                    for d in chars.by_ref() {
                        out.push(d);
                        if d.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
            continue;
        }
        let w = c.width().unwrap_or(0);
        if used + w > room {
            return (out, used);
        }
        out.push(c);
        used += w;
    }
    (out, used)
}

/// A call's heading on one line: `▸ title` in bold, and at the right edge
/// `16:53:29 · 13s · execute · completed`; the title wraps above it when
/// both don't fit.
fn header(title: &str, meta: &str, tail: &[(String, &str)], copy: Option<&str>, width: usize) -> String {
    let mut right = format!("{DIM}{meta}{RESET}");
    let mut right_len = meta.chars().count();
    for (text, colour) in tail {
        right.push_str(&format!("{DIM} · {RESET}{colour}{text}{RESET}"));
        right_len += 3 + text.chars().count();
    }
    // Cmd+click copies the command exactly as it ran, whatever it looks like here.
    if let Some(url) = copy {
        right.push_str(&format!("{DIM} · {RESET}{COPY_STYLE}{}{RESET}", axel_tui::util::hyperlink(url, COPY_LABEL)));
        right_len += 3 + COPY_LABEL.chars().count();
    }
    let head = format!("▸ {title}");
    let head_len = head.chars().count();
    if head_len + 2 + right_len <= width {
        let gap = width - head_len - right_len;
        return format!("{BOLD}{head}{RESET}{}{right}\r\n", " ".repeat(gap));
    }
    let mut out = wrap_plain(&head, width, "  ", BOLD);
    out.push_str(&format!("{}{right}\r\n", " ".repeat(width.saturating_sub(right_len))));
    out
}

/// A command as shell, except a heredoc's body, which is highlighted as what
/// reads it: `python3 - <<'EOF'` as Python, `cat > src/main.rs <<EOF` as Rust.
/// A command `width` wide, highlighted as shell. Too long for it, it is broken
/// across lines the way a person would write it ([`shell_lines`]).
fn command_block(cmd: &str, width: usize) -> String {
    let full = format!("$ {cmd}");
    let room = width.max(20);
    let bash = || Lang::Named("bash");
    let mut lines = Vec::new();
    match split_heredoc(&full) {
        Some(h) => {
            lines.extend(colour_lines(&shell_lines(h.head, room).join("\n"), bash()));
            // The body is text, not a command: wrapped here, between words.
            for line in colour_lines(&first_lines(h.body), h.lang) {
                lines.extend(hanging(&line, room));
            }
            // The terminator, as written, then what runs after it.
            let (end, after) = h.rest.split_once('\n').unwrap_or((h.rest, ""));
            if !end.is_empty() {
                lines.extend(colour_lines(end, bash()));
            }
            if !after.trim().is_empty() {
                lines.extend(colour_lines(&shell_lines(after.trim(), room).join("\n"), bash()));
            }
        }
        None => lines.extend(colour_lines(&shell_lines(&full, room).join("\n"), bash())),
    }
    emit(&lines, room, "")
}

/// A line of text wrapped to `room` between words, its wrapped lines
/// indented under its text: past its own indent and a list's `- ` or `1. `.
fn hanging(line: &[Cell], room: usize) -> Vec<Vec<Cell>> {
    let text: String = line.iter().map(|(c, _)| *c).collect();
    let rest = text.trim_start();
    let indent = text.chars().count() - rest.chars().count();
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let marker = if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
        2
    } else if digits > 0 && (rest[digits..].starts_with(". ") || rest[digits..].starts_with(") ")) {
        digits + 2
    } else {
        0
    };
    let hang = (indent + marker).min(room / 2);
    let Some(first) = word_chunks(line, room).first().map(|c| c.len()) else { return vec![line.to_vec()] };
    let mut out = vec![line[..first].to_vec()];
    for chunk in word_chunks(&line[first..], room.saturating_sub(hang).max(10)) {
        if chunk.is_empty() {
            continue;
        }
        let mut l: Vec<Cell> = vec![(' ', None); hang];
        l.extend_from_slice(chunk);
        out.push(l);
    }
    out
}

/// `cmd` as a person would write it out: one step per line, broken after
/// each `&&`, `||` and `;` (a shell reads on past an operator at the end of a
/// line, so it pastes as the same command, no backslashes). A pipeline stays on
/// its line when it fits `room`, else breaks after each `|`; a stage still too
/// long breaks between words with ` \`. A word longer than the line (a long
/// quoted string) stays whole, and a quoted string's own lines are its own.
fn shell_lines(cmd: &str, room: usize) -> Vec<String> {
    let steps = shell_steps(cmd);
    // Pipelines, and the operator that ends each.
    let mut groups: Vec<(Vec<&str>, &str)> = Vec::new();
    let mut stages = Vec::new();
    for (k, (_, step)) in steps.iter().enumerate() {
        stages.push(*step);
        let next = steps.get(k + 1).map_or("", |s| s.0);
        if next != "|" {
            groups.push((std::mem::take(&mut stages), next));
        }
    }
    let mut out = Vec::new();
    for (g, (stages, op)) in groups.iter().enumerate() {
        let indent = if g == 0 { "" } else { STEP_INDENT };
        let end = match *op {
            "" | "\n" => String::new(),
            ";" => ";".to_string(),
            op => format!(" {op}"),
        };
        let whole = format!("{indent}{}{end}", stages.join(" | "));
        if widest_line(&whole) <= room {
            out.push(whole);
            continue;
        }
        for (k, stage) in stages.iter().enumerate() {
            let lead = if k == 0 { indent.to_string() } else { format!("{indent}{STEP_INDENT}") };
            let tail = if k + 1 < stages.len() { " |".to_string() } else { end.clone() };
            out.extend(word_lines(stage, &lead, &tail, room));
        }
    }
    if out.is_empty() {
        out.push(cmd.to_string());
    }
    out
}

/// How far a step's lines are indented under the first.
const STEP_INDENT: &str = "  ";

/// The widest of a text's lines.
fn widest_line(s: &str) -> usize {
    s.lines().map(|l| l.chars().count()).max().unwrap_or(0)
}

/// One stage between words, `lead` before it, `tail` after it, lines but the
/// last ending in ` \` and the rest indented under the first.
fn word_lines(stage: &str, lead: &str, tail: &str, room: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = lead.to_string();
    let mut fresh = true;
    let last_len = |s: &str| s.rsplit('\n').next().map_or(0, |l| l.chars().count());
    let words = shell_words(stage);
    for (k, word) in words.iter().enumerate() {
        let first_len = word.split('\n').next().map_or(0, |l| l.chars().count());
        // The last word is followed by `tail`, the others by room for a ` \`.
        let after = if k + 1 == words.len() { tail.chars().count() } else { 2 };
        if !fresh && last_len(&cur) + 1 + first_len + after > room {
            cur.push_str(" \\");
            lines.push(std::mem::take(&mut cur));
            cur = format!("{lead}    ");
        } else if !fresh {
            cur.push(' ');
        }
        cur.push_str(word);
        fresh = false;
    }
    cur.push_str(tail);
    lines.push(cur);
    lines
}

/// Where a shell reads a quote, an escape, or a level of parentheses.
#[derive(Clone, Copy, Default)]
struct ShellScan {
    single: bool,
    double: bool,
    escaped: bool,
    depth: usize,
}

impl ShellScan {
    /// Takes `c`; whether it was outside quotes and parentheses (and not escaped).
    fn top(&mut self, c: char) -> bool {
        if self.escaped {
            self.escaped = false;
            return false;
        }
        match c {
            '\\' if !self.single => {
                self.escaped = true;
                false
            }
            '\'' if !self.double => {
                self.single = !self.single;
                false
            }
            '"' if !self.single => {
                self.double = !self.double;
                false
            }
            _ if self.single || self.double => false,
            '(' => {
                self.depth += 1;
                false
            }
            ')' => {
                self.depth = self.depth.saturating_sub(1);
                false
            }
            _ => self.depth == 0,
        }
    }
}

/// A command's steps and the operator before each (`""` for the first):
/// `a && b | c` → `("", "a"), ("&&", "b"), ("|", "c")`.
fn shell_steps(cmd: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut scan = ShellScan::default();
    let (mut start, mut op) = (0, "");
    let chars: Vec<(usize, char)> = cmd.char_indices().collect();
    let mut k = 0;
    while k < chars.len() {
        let (i, c) = chars[k];
        let next = chars.get(k + 1).map(|(_, c)| *c);
        let prev = if k > 0 { Some(chars[k - 1].1) } else { None };
        if scan.top(c) {
            let found = match (c, next) {
                ('&', Some('&')) => Some("&&"),
                ('|', Some('|')) => Some("||"),
                // `|&` and `>|` aren't steps.
                ('|', n) if n != Some('&') && prev != Some('>') => Some("|"),
                (';', n) if n != Some(';') => Some(";"),
                ('\n', _) => Some("\n"),
                _ => None,
            };
            if let Some(found) = found {
                out.push((op, cmd[start..i].trim()));
                op = found;
                start = i + found.len();
                k += found.len();
                continue;
            }
        }
        k += 1;
    }
    out.push((op, cmd[start..].trim()));
    out.retain(|(_, s)| !s.is_empty());
    out
}

/// A step's words, split at spaces outside quotes and parentheses.
fn shell_words(step: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut scan = ShellScan::default();
    let mut start = None;
    for (i, c) in step.char_indices() {
        let top = scan.top(c);
        if top && c.is_whitespace() {
            if let Some(s) = start.take() {
                out.push(&step[s..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push(&step[s..]);
    }
    out
}


/// A command with a heredoc: the line that opens it, the body, the rest
/// (from the terminator on), and what language the body is.
#[derive(Debug, PartialEq)]
struct Heredoc<'a> {
    head: &'a str,
    body: &'a str,
    rest: &'a str,
    lang: Lang<'a>,
}

fn split_heredoc(cmd: &str) -> Option<Heredoc<'_>> {
    let (head, after) = cmd.split_once('\n')?;
    let at = head.find("<<")?;
    let tag_part = head[at + 2..].trim_start_matches('-').trim_start();
    let tag: String = tag_part.trim_start_matches(['\'', '"']).chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    if tag.is_empty() {
        return None;
    }
    // The body runs to a line that is just the tag.
    let mut end = after.len();
    let mut pos = 0;
    for line in after.split_inclusive('\n') {
        if line.trim() == tag {
            end = pos;
            break;
        }
        pos += line.len();
    }
    let body = after[..end].trim_end_matches('\n');
    let rest = after[end..].trim_end_matches('\n');
    Some(Heredoc { head, body, rest, lang: heredoc_lang(&head[..at]) })
}

/// What reads a heredoc, from the command before `<<`: an interpreter, or a
/// file it is written to (`cat > f`, `tee f`).
fn heredoc_lang(before: &str) -> Lang<'_> {
    // The last command of a chain (`cd x && python3 - <<EOF`).
    let last = before.rsplit(['&', ';', '|']).next().unwrap_or(before).trim();
    let words: Vec<&str> = last.split_whitespace().collect();
    if let Some(i) = words.iter().position(|w| *w == ">" || *w == ">>")
        && let Some(path) = words.get(i + 1)
    {
        return Lang::File(path.trim_matches(['\'', '"']));
    }
    if let Some(w) = words.iter().position(|w| *w == "tee").and_then(|i| words.get(i + 1)) {
        return Lang::File(w.trim_matches(['\'', '"']));
    }
    let program = words.first().map(|w| w.rsplit('/').next().unwrap_or(w)).unwrap_or("");
    match program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.') {
        "python" => Lang::Named("python"),
        "node" | "deno" | "bun" => Lang::Named("js"),
        "ruby" => Lang::Named("ruby"),
        "perl" => Lang::Named("perl"),
        "sh" | "bash" | "zsh" => Lang::Named("bash"),
        "psql" | "sqlite" | "mysql" => Lang::Named("sql"),
        _ => Lang::Named("txt"),
    }
}

/// The file a command does nothing but print (`cat f`, `sed -n 1,40p f`,
/// `head -50 f | tail -20` after a `cd`), so its output is that file's text;
/// several such reads chained (`sed … a.rs; sed … b.rs`) when the files share
/// a language. `None` for anything else: a search, a pipe into more, files
/// of different languages.
fn file_printed_by(cmd: &str) -> Option<&str> {
    // Quotes would need a shell's parsing to find the file: not worth guessing.
    if cmd.contains('\n') || cmd.contains("<<") || cmd.contains(['\'', '"']) {
        return None;
    }
    let ext = |f: &str| f.rsplit_once('.').map(|(_, e)| e.to_string());
    let mut files: Vec<&str> = Vec::new();
    for step in cmd.split("&&").flat_map(|s| s.split(';')).map(str::trim).filter(|s| !s.starts_with("cd ") && !s.is_empty()) {
        files.push(file_read_by(step)?);
    }
    let first = *files.first()?;
    files.iter().all(|f| ext(f) == ext(first)).then_some(first)
}

/// The one file a single step of a command prints, or `None`.
fn file_read_by(step: &str) -> Option<&str> {
    let mut stages = step.split('|').map(str::trim);
    let first = stages.next()?;
    // Only trimming after it: `| head`, `| tail`.
    if !stages.all(|s| matches!(s.split_whitespace().next(), Some("head" | "tail"))) {
        return None;
    }
    let words: Vec<&str> = first.split_whitespace().collect();
    if !matches!(words.first().copied(), Some("cat" | "head" | "tail" | "sed" | "nl" | "less" | "more" | "bat")) {
        return None;
    }
    let files: Vec<&str> = words[1..]
        .iter()
        .copied()
        .filter(|w| !w.starts_with('-') && w.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric())))
        .collect();
    match files.as_slice() {
        [one] => Some(one),
        _ => None,
    }
}

/// The first [`MAX_LINES`] lines, and how many more there were.
fn first_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut s = lines.iter().take(MAX_LINES).copied().collect::<Vec<_>>().join("\n");
    if lines.len() > MAX_LINES {
        s.push_str(&format!("\n… {} more lines", lines.len() - MAX_LINES));
    }
    s
}

/// What a read returned without the line numbers Claude puts in front
/// (`    12→fn main() {`), or `None` when it isn't shaped like that.
fn strip_line_numbers(text: &str) -> Option<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let (n, rest) = line.split_once('→')?;
        if n.trim().parse::<u64>().is_err() {
            return None;
        }
        out.push(rest);
    }
    (!out.is_empty()).then(|| out.join("\n"))
}

/// How to pick a syntax: by a file's name, or by a language's name.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Lang<'a> {
    File(&'a str),
    Named(&'a str),
}

/// syntect's bundled syntaxes, loaded once.
fn syntaxes() -> &'static syntect::parsing::SyntaxSet {
    static SET: std::sync::OnceLock<syntect::parsing::SyntaxSet> = std::sync::OnceLock::new();
    // syntect's own set and `syntaxes/` (Swift, TypeScript, TOML…), compiled by build.rs.
    static DUMP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/syntaxes.packdump"));
    SET.get_or_init(|| syntect::dumps::from_binary(DUMP))
}

/// The syntax for a file (by its extension, or its name: `Makefile`) or a
/// language (`bash`, `json`).
fn syntax_for(lang: Lang) -> Option<&'static syntect::parsing::SyntaxReference> {
    let set = syntaxes();
    match lang {
        Lang::File(path) => {
            let name = path.rsplit('/').next().unwrap_or(path);
            let ext = name.rsplit_once('.').map_or(name, |(_, e)| e);
            set.find_syntax_by_extension(ext).or_else(|| set.find_syntax_by_extension(name))
        }
        Lang::Named(name) => set.find_syntax_by_token(name),
    }
}

/// The terminal colour (an SGR code, so it takes the terminal's palette, as
/// bat's `ansi` theme does) for the innermost scope that has one.
fn scope_colour(stack: &syntect::parsing::ScopeStack) -> Option<&'static str> {
    for scope in stack.as_slice().iter().rev() {
        let s = scope.build_string();
        let has = |p: &str| s == p || s.starts_with(&format!("{p}."));
        let code = if has("comment") {
            "2"
        } else if has("markup.heading") || has("markup.bold") {
            "1"
        } else if has("markup.italic") {
            "3"
        } else if has("markup.raw") {
            "36"
        } else if has("markup.underline.link") || has("meta.link") {
            "34"
        } else if has("markup.inserted") {
            "32"
        } else if has("markup.deleted") {
            "31"
        } else if has("string") {
            "32"
        } else if has("constant.numeric") || has("constant.language") || has("constant.character") || has("entity.other.attribute-name") {
            "33"
        } else if has("keyword") || has("storage") {
            "35"
        } else if has("entity.name.function") || has("support.function") || has("variable.function") || has("entity.name.tag") {
            "34"
        } else if has("entity.name.type") || has("entity.name.class") || has("entity.name.struct") || has("entity.name.enum") || has("support.type") || has("support.class") {
            "36"
        } else {
            continue;
        };
        return Some(code);
    }
    None
}

/// `code` highlighted by its language (syntect's bundled syntaxes, in the
/// terminal's own colours), wrapped to `width`, each line indented and led
/// by `mark` (a diff's `-` / `+`). Plain when no syntax matches.
fn highlighted(code: &str, lang: Lang, width: usize, mark: &str) -> String {
    emit(&colour_lines(code, lang), width, mark)
}

/// A character and its colour (an SGR code), for [`emit`].
type Cell = (char, Option<&'static str>);

/// Coloured lines as terminal output: wrapped to `width` between words,
/// each line indented and led by `mark`.
fn emit(lines: &[Vec<Cell>], width: usize, mark: &str) -> String {
    emit_linked(lines, &[], width, mark, None)
}

/// A run of a line's characters (`from..to`) that links to `url`.
#[derive(Clone, Debug, PartialEq)]
struct Link {
    from: usize,
    to: usize,
    url: String,
}

/// [`emit`], with `links[i]` the links on `lines[i]` as terminal hyperlinks
/// (closed at the end of each wrapped piece and reopened on the next), and
/// on `bg` (an SGR background) to the edge of `width` when there is one.
fn emit_linked(lines: &[Vec<Cell>], links: &[Vec<Link>], width: usize, mark: &str, bg: Option<&str>) -> String {
    let on = |out: &mut String| {
        out.push_str(RESET);
        if let Some(bg) = bg {
            out.push_str(&format!("\x1b[{bg}m"));
        }
    };
    // A mark leads each line; a background block gets a space at its end too.
    let room = width.saturating_sub(mark.chars().count() + usize::from(bg.is_some() && !mark.is_empty())).max(10);
    let mut out = String::new();
    for (n, line) in lines.iter().enumerate() {
        let line_links = links.get(n).map(Vec::as_slice).unwrap_or(&[]);
        // Trailing spaces (git pads its `remote:` lines) would wrap onto a line of their own.
        let line = &line[..line.iter().rposition(|(c, _)| !c.is_whitespace()).map_or(0, |i| i + 1)];
        let mut at = 0;
        for chunk in word_chunks(line, room) {
            out.push_str(INDENT);
            on(&mut out);
            out.push_str(mark);
            let mut cur: Option<&str> = None;
            let mut open: Option<&str> = None;
            for &(c, colour) in chunk {
                let link = line_links.iter().find(|l| (l.from..l.to).contains(&at)).map(|l| l.url.as_str());
                if link != open {
                    out.push_str(&format!("\x1b]8;;{}\x1b\\", link.unwrap_or("")));
                    open = link;
                }
                if colour != cur {
                    on(&mut out);
                    if let Some(code) = colour {
                        out.push_str(&format!("\x1b[{code}m"));
                    }
                    cur = colour;
                }
                out.push(c);
                at += 1;
            }
            if open.is_some() {
                out.push_str("\x1b]8;;\x1b\\");
            }
            if bg.is_some() {
                on(&mut out);
                out.push_str(&" ".repeat(room.saturating_sub(chunk.len()) + usize::from(!mark.is_empty())));
            }
            out.push_str(RESET);
            out.push_str("\r\n");
        }
    }
    out
}

/// A line of a diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Same,
    Removed,
    Added,
}

/// Background of a removed line (a dark red) and of an added one (a dark
/// green), as GitHub's dark theme shows a diff.
const REMOVED_BG: &str = "48;2;63;22;27";
const ADDED_BG: &str = "48;2;18;54;33";

/// `old` → `new` line by line: what stayed, what went, what came, in order
/// (a longest-common-subsequence diff). Each line comes with its index in
/// `old` (Same, Removed) or `new` (Added). A very large edit is shown as all
/// removed then all added.
fn diff_lines(old: &[&str], new: &[&str]) -> Vec<(Change, usize)> {
    let (n, m) = (old.len(), new.len());
    if n * m > 250_000 {
        return (0..n).map(|i| (Change::Removed, i)).chain((0..m).map(|j| (Change::Added, j))).collect();
    }
    // lcs[i][j]: the longest common run of old[i..] and new[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old[i] == new[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < n || j < m {
        if i < n && j < m && old[i] == new[j] {
            out.push((Change::Same, i));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            // On a tie the removal comes first: `-` lines above their `+`.
            out.push((Change::Removed, i));
            i += 1;
        } else {
            out.push((Change::Added, j));
            j += 1;
        }
    }
    out
}

/// An edit as a diff: unchanged lines once, as context; removed lines on a
/// dark red background, added ones on a dark green, across the pane; each
/// side highlighted in its language.
fn diff_view(old: &str, new: &str, lang: Lang, width: usize) -> String {
    let (old_lines, new_lines): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let (old_cells, new_cells) = (colour_lines(old, lang), colour_lines(new, lang));
    let room = width.saturating_sub(2).max(10);
    let mut out = String::new();
    for (change, k) in diff_lines(&old_lines, &new_lines) {
        let (cells, mark, bg) = match change {
            Change::Same => (&old_cells[k], ("  ", None), None),
            Change::Removed => (&old_cells[k], ("- ", Some("31")), Some(REMOVED_BG)),
            Change::Added => (&new_cells[k], ("+ ", Some("32")), Some(ADDED_BG)),
        };
        for (n, chunk) in word_chunks(cells, room).into_iter().enumerate() {
            out.push_str(INDENT);
            let sgr = |fg: Option<&str>| -> String {
                let codes: Vec<&str> = [fg, bg].into_iter().flatten().collect();
                if codes.is_empty() { RESET.to_string() } else { format!("{RESET}\x1b[{}m", codes.join(";")) }
            };
            out.push_str(&sgr(mark.1));
            // A wrapped line's continuation keeps the background, not the mark.
            out.push_str(if n == 0 { mark.0 } else { "  " });
            let mut cur: Option<Option<&str>> = None;
            for &(c, colour) in chunk {
                if cur != Some(colour) {
                    out.push_str(&sgr(colour));
                    cur = Some(colour);
                }
                out.push(c);
            }
            // The background runs to the edge of the pane.
            if bg.is_some() {
                out.push_str(&sgr(None));
                out.push_str(&" ".repeat(room.saturating_sub(chunk.len())));
            }
            out.push_str(RESET);
            out.push_str("\r\n");
        }
    }
    out
}

/// One line of `grep -n` / `rg -n` output.
#[derive(Debug, PartialEq)]
enum GrepLine<'a> {
    /// `path:12:text` (a match) or `path-12-text` (context); the path is
    /// `None` when one file was searched (`12:text`).
    /// `outer` is the number a second, piped `grep -n` put in front
    /// (`5:92-text`: line 92 of the file, line 5 of what the second grep read).
    /// `sep` is the separator as grep printed it (`:` a match, `-` context);
    /// `matched` whether to show it as a match.
    Hit { path: Option<&'a str>, outer: Option<&'a str>, line: &'a str, sep: char, matched: bool, text: &'a str },
    /// `--` between groups of context.
    Gap,
}

fn parse_grep_line(l: &str) -> Option<GrepLine<'_>> {
    if l == "--" {
        return Some(GrepLine::Gap);
    }
    // `12:text` / `12-text`
    let digits = l.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && matches!(l.as_bytes().get(digits), Some(b':' | b'-')) {
        let matched = l.as_bytes()[digits] == b':';
        let (line, rest) = (&l[..digits], &l[digits + 1..]);
        // `5:92-text`: a piped `grep -n` numbered the first one's lines again.
        // The second grep decides what is shown: its `:` makes the line a match.
        if let Some(GrepLine::Hit { path: None, outer: None, line: inner, sep, text, .. }) = parse_grep_line(rest).filter(|_| !rest.starts_with(char::is_whitespace)) {
            return Some(GrepLine::Hit { path: None, outer: Some(line), line: inner, sep, matched, text });
        }
        let sep = if matched { ':' } else { '-' };
        return Some(GrepLine::Hit { path: None, outer: None, line, sep, matched, text: rest });
    }
    // `path:12:text` / `path-12-text`: the first `:N:` or `-N-` after a path.
    for (i, sep) in l.char_indices().filter(|(_, c)| *c == ':' || *c == '-') {
        let rest = &l[i + 1..];
        let d = rest.chars().take_while(char::is_ascii_digit).count();
        if i > 0 && d > 0 && rest[d..].starts_with(sep) {
            let path = &l[..i];
            if path.contains(char::is_whitespace) {
                return None;
            }
            return Some(GrepLine::Hit { path: Some(path), outer: None, line: &rest[..d], sep, matched: sep == ':', text: &rest[d + 1..] });
        }
    }
    None
}

/// The words a `grep`/`rg` pattern looks for, when it is plain words (alternatives
/// `a\|b` allowed): what to mark in the results. Empty for a real regex.
fn grep_terms(cmd: &str) -> Vec<String> {
    let Some(at) = cmd.find("grep ").or_else(|| cmd.find("rg ")) else { return Vec::new() };
    let rest = &cmd[at..];
    let Some(q) = rest.find(['"', '\'']) else { return Vec::new() };
    let quote = rest.as_bytes()[q] as char;
    let Some(len) = rest[q + 1..].find(quote) else { return Vec::new() };
    let pattern = &rest[q + 1..q + 1 + len];
    let parts: Vec<String> = pattern.split("\\|").flat_map(|p| p.split('|')).map(|p| p.trim_matches(['^', '$']).replace("\\(", "(").replace("\\)", ")")).collect();
    if parts.iter().any(|p| p.is_empty() || p.contains(['.', '*', '+', '?', '[', ']', '{', '}', '\\'])) {
        return Vec::new();
    }
    parts
}

/// The output of a search, ripgrep-style: paths magenta, line numbers green,
/// each line's text highlighted in its file's language (context lines dim),
/// and the search terms bold red. `None` when it doesn't read as grep output.
fn grep_view(cmd: &str, output: &str, width: usize, links: &Linker) -> Option<String> {
    if !(cmd.contains("grep ") || cmd.contains("rg ")) {
        return None;
    }
    let lines: Vec<&str> = output.lines().take(MAX_LINES).collect();
    let mut parsed: Vec<Option<GrepLine>> = lines.iter().map(|l| parse_grep_line(l)).collect();
    let hits = parsed.iter().filter(|p| p.is_some()).count();
    if hits * 2 < lines.iter().filter(|l| !l.trim().is_empty()).count() {
        // Mostly something else (a file printed, then a search): only
        // `path:12:` lines are surely results; the rest stays as it was.
        parsed.iter_mut().filter(|p| !matches!(p, Some(GrepLine::Hit { path: Some(_), .. }))).for_each(|p| *p = None);
    }
    if parsed.iter().all(Option::is_none) {
        return None;
    }
    let one_file = file_searched(cmd);
    // A bare line number opens a file only when there was one to search.
    let link_file = only_file(cmd);
    let terms = grep_terms(cmd);
    // Paths are relative to where the search ran (`cd crates && grep …`).
    let links = links.after_cd(cmd);
    // Bare line numbers right-aligned, so the text lines up.
    let digits = parsed.iter().filter_map(|p| match p {
        Some(GrepLine::Hit { path: None, line, .. }) => Some(line.len()),
        _ => None,
    });
    let num_width = digits.max().unwrap_or(0);
    let mut out: Vec<Vec<Cell>> = Vec::new();
    let mut spans: Vec<Vec<Link>> = Vec::new();
    let mut last_line: Option<usize> = None;
    for (raw, p) in lines.iter().zip(parsed) {
        let mut cells: Vec<Cell> = Vec::new();
        let mut line_links = Vec::new();
        let mut gutter_at: Option<usize> = None;
        // Without paths, the next search's results start back at a lower line: a gap between them.
        if let Some(GrepLine::Hit { path: None, line, .. }) = &p {
            let n = line.parse().ok();
            if let (Some(prev), Some(n)) = (last_line, n)
                && n < prev
            {
                out.push(Vec::new());
                spans.push(Vec::new());
            }
            last_line = n;
        } else if matches!(p, Some(GrepLine::Gap)) {
            last_line = None;
        }
        match p {
            Some(GrepLine::Gap) => cells.extend("--".chars().map(|c| (c, Some("2")))),
            // A piped `grep -n`'s own numbering (`5:92-`) says nothing about the file: not shown.
            Some(GrepLine::Hit { path, outer: _, line, sep, matched, text }) => {
                if let Some(path) = path {
                    cells.extend(path.chars().map(|c| (c, Some("35"))));
                    cells.push((sep, Some("2")));
                } else {
                    cells.extend(std::iter::repeat_n((' ', None), num_width.saturating_sub(line.len())));
                }
                // `path:12` (or just `12`, when one file was searched) opens the file there.
                let from = if path.is_some() { 0 } else { cells.len() };
                cells.extend(line.chars().map(|c| (c, Some("32"))));
                if let Some(url) = path.or(link_file).and_then(|f| links.url(f, line.parse().ok())) {
                    line_links.push(Link { from, to: cells.len(), url });
                }
                // `:` a match, `-` a line around one.
                cells.push((if matched { ':' } else { '-' }, Some("2")));
                // …and past the code's own indent.
                gutter_at = Some(cells.len() + text.chars().take_while(|c| *c == ' ').count());
                let _ = sep;
                let file = path.or(one_file);
                let mut code: Vec<Cell> = match file {
                    Some(f) => colour_lines(text, Lang::File(f)).into_iter().next().unwrap_or_default(),
                    None => text.chars().map(|c| (c, None)).collect(),
                };
                if !matched {
                    code.iter_mut().for_each(|cell| cell.1 = Some("2"));
                }
                mark_terms(&mut code, &terms);
                cells.extend(code);
            }
            None => cells.extend(raw.chars().map(|c| (c, Some("2")))),
        }
        // Wrapped, a result's text hangs under its text, past `191-`.
        let gutter = match &gutter_at {
            Some(g) => (*g).min(width / 2),
            None => 0,
        };
        let pieces = word_chunks(&cells, width.max(10));
        let first = pieces.first().map_or(0, |c| c.len());
        out.push(cells[..first].to_vec());
        spans.push(line_links);
        if first < cells.len() {
            for chunk in word_chunks(&cells[first..], width.saturating_sub(gutter).max(10)) {
                let mut l: Vec<Cell> = vec![(' ', None); gutter];
                l.extend_from_slice(chunk);
                out.push(l);
                spans.push(Vec::new());
            }
        }
    }
    Some(emit_linked(&out, &spans, width, "", None))
}

/// A file whose language the results without a path are in: the files the
/// command searches, when they all share an extension (`a.rs`, `b/mod.rs`).
fn file_searched(cmd: &str) -> Option<&str> {
    let ext = |w: &str| w.rsplit_once('.').filter(|(s, e)| !s.is_empty() && !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric())).map(|(_, e)| e.to_string());
    // A command separator can stick to the word before it (`mod.rs;`).
    let files: Vec<&str> = cmd.split_whitespace().map(|w| w.trim_end_matches([';', '|', '&', ')'])).filter(|w| !w.starts_with('-') && !w.contains(['"', '\'']) && ext(w).is_some()).collect();
    let first = *files.first()?;
    files.iter().all(|f| ext(f) == ext(first)).then_some(first)
}

/// The one file a command searches, when it names exactly one (`grep -n x
/// a.rs | grep -n y` searches `a.rs`; `grep x a.rs; grep y b.rs` names two).
fn only_file(cmd: &str) -> Option<&str> {
    let first = file_searched(cmd)?;
    let ext = first.rsplit_once('.').map(|(_, e)| e)?;
    let mut names: Vec<&str> = cmd
        .split_whitespace()
        .map(|w| w.trim_end_matches([';', '|', '&', ')']))
        .filter(|w| !w.starts_with('-') && !w.contains(['"', '\'']) && w.ends_with(&format!(".{ext}")))
        .collect();
    names.dedup();
    names.sort_unstable();
    names.dedup();
    (names.len() == 1).then_some(first)
}

/// Every occurrence of a term, bold red.
fn mark_terms(cells: &mut [Cell], terms: &[String]) {
    let text: Vec<char> = cells.iter().map(|(c, _)| *c).collect();
    for term in terms {
        let t: Vec<char> = term.chars().collect();
        if t.is_empty() || t.len() > text.len() {
            continue;
        }
        let mut i = 0;
        while i + t.len() <= text.len() {
            if text[i..i + t.len()] == t[..] {
                cells[i..i + t.len()].iter_mut().for_each(|c| c.1 = Some("1;31"));
                i += t.len();
            } else {
                i += 1;
            }
        }
    }
}

/// A highlighted line in pieces of at most `room` characters, broken after
/// the last space that fits (unless that leaves a piece mostly empty: a long
/// token breaks where it must).
fn word_chunks<T>(line: &[(char, T)], room: usize) -> Vec<&[(char, T)]> {
    if line.is_empty() {
        return vec![line];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < line.len() {
        let mut take = room.min(line.len() - i);
        if i + take < line.len()
            && let Some(sp) = line[i..i + take].iter().rposition(|(c, _)| *c == ' ')
            && sp >= room / 2
        {
            take = sp + 1;
        }
        out.push(&line[i..i + take]);
        i += take;
    }
    out
}

/// Each line of `code` as characters with their colours.
fn colour_lines(code: &str, lang: Lang) -> Vec<Vec<(char, Option<&'static str>)>> {
    let plain = || code.lines().map(|l| l.chars().map(|c| (c, None)).collect()).collect();
    let Some(syntax) = syntax_for(lang) else { return plain() };
    let mut state = syntect::parsing::ParseState::new(syntax);
    let mut stack = syntect::parsing::ScopeStack::new();
    let mut out = Vec::new();
    for line in code.lines() {
        let with_nl = format!("{line}\n");
        let Ok(ops) = state.parse_line(&with_nl, syntaxes()) else { return plain() };
        let mut cells = Vec::with_capacity(line.len());
        let mut at = 0;
        for (pos, op) in ops {
            let pos = pos.min(line.len());
            if pos > at {
                let colour = scope_colour(&stack);
                cells.extend(line[at..pos].chars().map(|c| (c, colour)));
                at = pos;
            }
            if stack.apply(&op).is_err() {
                return plain();
            }
        }
        let colour = scope_colour(&stack);
        cells.extend(line[at..].chars().map(|c| (c, colour)));
        out.push(cells);
    }
    out
}

/// `text` wrapped to `width` (by characters, so paths and commands break
/// anywhere), each line led by `indent` (a title's continuation lines by
/// `indent` alone, its first by nothing), all in `style`.
fn wrap_plain(text: &str, width: usize, indent: &str, style: &str) -> String {
    wrap_linked(text, None, width, indent, style)
}

/// [`wrap_plain`], each line's text a hyperlink to `url` when there is one.
fn wrap_linked(text: &str, url: Option<&str>, width: usize, indent: &str, style: &str) -> String {
    let title = indent == "  ";
    let mut out = String::new();
    for (n, para) in text.lines().enumerate() {
        let chars: Vec<char> = para.chars().collect();
        let mut pfx = if title && n == 0 { "" } else { indent };
        let mut i = 0;
        loop {
            let room = width.saturating_sub(pfx.chars().count()).max(10);
            let mut take = room.min(chars.len() - i);
            // Break after the last space that fits, unless that leaves the line
            // mostly empty (a long path or token breaks where it must).
            if i + take < chars.len()
                && let Some(sp) = chars[i..i + take].iter().rposition(|c| *c == ' ')
                && sp >= room / 2
            {
                take = sp + 1;
            }
            let piece: String = chars[i..i + take].iter().collect();
            let piece = piece.trim_end();
            match url {
                Some(url) => out.push_str(&format!("{pfx}{style}{}{RESET}\r\n", axel_tui::util::hyperlink(url, piece))),
                None => out.push_str(&format!("{pfx}{style}{piece}{RESET}\r\n")),
            }
            i += take;
            if i >= chars.len() {
                break;
            }
            pfx = indent;
        }
    }
    out
}

/// Older session hosts cut a long output as its JSON, so it was stored as
/// `"line\n  more…` or `[{"type":"text","text":"line\n…`: the text back,
/// escapes decoded. `None` for anything that isn't shaped like that.
fn unescape_cut_json(s: &str) -> Option<String> {
    let body = if let Some(rest) = s.strip_prefix('"') {
        rest
    } else if s.starts_with("[{") {
        &s[s.find("\"text\":\"")? + 8..]
    } else {
        return None;
    };
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => {}
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).unwrap_or('?'));
                }
                other => out.push(other),
            },
            // The string's own closing quote: the rest is the JSON around it.
            '"' => break,
            c => out.push(c),
        }
    }
    let out = out.trim_end_matches('…').to_string();
    (!out.is_empty()).then(|| format!("{out}…"))
}

/// A tool's output as text: a string, text blocks (`[{type: text, text}]`), or JSON.
fn output_text(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => unescape_cut_json(s).unwrap_or_else(|| s.clone()),
        Some(serde_json::Value::Array(a)) if a.iter().all(|b| b.get("text").is_some()) => {
            a.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n")
        }
        Some(v) if !v.is_null() => serde_json::to_string_pretty(v).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Wait for Esc, q or Enter (raw, so nothing echoes into the output):
/// `None` then. A resize returns the width it settles on (a drag sends many).
fn wait_for_close_or_resize() -> Option<usize> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, poll, read};
    use std::time::Duration;
    let _ = crossterm::terminal::enable_raw_mode();
    let mut resized: Option<u16> = None;
    let result = loop {
        // A resize counts once the size has held for a moment.
        let wait = if resized.is_some() { Duration::from_millis(250) } else { Duration::from_secs(3600) };
        match poll(wait) {
            Ok(true) => match read() {
                Ok(Event::Key(k)) if k.kind != KeyEventKind::Release && matches!(k.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) => break None,
                Ok(Event::Resize(w, _)) => resized = Some(w),
                Ok(_) => {}
                Err(_) => break None,
            },
            Ok(false) => {
                if let Some(w) = resized {
                    break Some(usize::from(w).max(20));
                }
            }
            Err(_) => break None,
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    result
}

/// The tool calls pane: down the right of the window, above the composer,
/// so the conversation stays readable beside it.
const TOOLS_BOUNDS: [f64; 4] = [0.655, 0.015, 0.33, 0.82];
const TOOLS_LABEL: &str = "tool calls";

#[cfg(test)]
mod tests {
    use super::*;

    /// Without colours or carriage returns, as the text reads.
    pub(super) fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&']') {
                // OSC (a hyperlink): up to its ST, `ESC \`.
                while let Some(d) = chars.next() {
                    if d == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            } else if c == '\x1b' {
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else if c != '\r' {
                out.push(c);
            }
        }
        out
    }

    fn no_links() -> Linker {
        Linker::new(None, Editor::File)
    }

    /// The hyperlinks in `s`, in order: `(url, text)`.
    fn hyperlinks(s: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(at) = rest.find("\x1b]8;;") {
            let after = &rest[at + 5..];
            let Some(end) = after.find("\x1b\\") else { break };
            let url = &after[..end];
            let body = &after[end + 2..];
            let close = body.find("\x1b]8;;").unwrap_or(body.len());
            if !url.is_empty() {
                out.push((url.to_string(), plain(&body[..close])));
            }
            rest = &body[close..];
        }
        out
    }

    /// What the shell runs, given the lines as pasted: an operator at the
    /// end of a line, or a ` \`, carries on to the next.
    fn pasted(lines: &[String]) -> String {
        lines.join("\n").replace(" \\\n", " ").split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn words(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn a_command_breaks_after_each_step() {
        assert_eq!(shell_lines("$ ls -la", 80), vec!["$ ls -la"], "one step, one line");
        // Every &&, || and ; ends a line, even when it would all fit.
        assert_eq!(shell_lines("$ cd /w && cargo fmt || true; ls", 80), vec!["$ cd /w &&", "  cargo fmt ||", "  true;", "  ls"]);
        // A pipeline stays together when it fits, and breaks after each | when not.
        let cmd = r#"$ cd /Users/ludo/Coding/pay-ledger-send/rust && grep -rn "send_stablecoin\|StablecoinSendRequest {" crates/ | grep -v "^crates/core/src/client/send.rs" ; grep -n "a" crates/core/src/client/send.rs 2>&1 | sed -n 1,40p"#;
        let lines = shell_lines(cmd, 80);
        assert_eq!(
            lines,
            vec![
                "$ cd /Users/ludo/Coding/pay-ledger-send/rust &&",
                r#"  grep -rn "send_stablecoin\|StablecoinSendRequest {" crates/ |"#,
                r#"    grep -v "^crates/core/src/client/send.rs";"#,
                r#"  grep -n "a" crates/core/src/client/send.rs 2>&1 | sed -n 1,40p"#,
            ],
            "not at the | inside quotes, nor at 2>&1"
        );
        assert_eq!(pasted(&lines).replace("\"; ", "\" ; "), words(cmd), "pastes as the same command");
        assert!(lines.iter().all(|l| l.chars().count() <= 80));

        // A commit message's own lines stay as written; the step after it still breaks.
        let commit = "$ git add -A rust && git commit -q -m \"fix(send): wait\n\npay send waits for the device.\" && git push -q origin HEAD 2>&1 | tail -3; git log --oneline -4";
        let lines = shell_lines(commit, 80);
        assert_eq!(lines, vec!["$ git add -A rust &&", "  git commit -q -m \"fix(send): wait\n\npay send waits for the device.\" &&", "  git push -q origin HEAD 2>&1 | tail -3;", "  git log --oneline -4"]);

        // The operator after a step counts toward its line.
        let tight = "$ cd /w &&\n  git add rust/crates/cli/src/main.rs rust/crates/cli/src/commands/help.rs && git log";
        let lines = shell_lines(&tight.replace("\n  ", " "), 76);
        assert!(lines.iter().all(|l| l.chars().count() <= 76), "{lines:#?}");
        assert!(lines.iter().all(|l| l.trim() != "&&"), "an operator never sits alone: {lines:#?}");

        // A stage too long on its own breaks between words; a quoted string stays whole.
        let long = format!(r#"$ cargo clippy -p pay -p pay-core --tests --all-features -- -D warnings -W clippy::pedantic "{}""#, "x y".repeat(20));
        let lines = shell_lines(&long, 50);
        assert!(lines.len() > 2 && lines[1].starts_with("    ") && lines[0].ends_with(" \\"), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains(&"x y".repeat(20))), "the quoted string isn't split");
        assert_eq!(pasted(&lines), words(&long));

        assert_eq!(shell_steps("(cd a && make) || echo $(a | b); x"), vec![("", "(cd a && make)"), ("||", "echo $(a | b)"), (";", "x")], "parentheses are one step");
        assert_eq!(shell_steps("a\nb"), vec![("", "a"), ("\n", "b")], "a newline ends a step");
    }

    #[test]
    fn a_command_has_a_link_that_copies_it_as_it_ran() {
        let cmd = "cd /w &&\n  cargo test";
        let it = item("01ABC", axel_store::ItemKind::ToolCall, serde_json::json!({ "title": "t", "kind": "execute", "raw_input": { "command": "cd /w && cargo test" } }));
        let shown = describe_call(&it, None, 100, &no_links());
        assert_eq!(hyperlinks(&shown), vec![("axel-links://copy?item=01ABC".to_string(), COPY_LABEL.to_string())], "{shown:?}");
        assert!(plain(&shown).lines().nth(1).unwrap().trim_end().ends_with(&format!("· {COPY_LABEL}")), "at the end of the header: {}", plain(&shown));
        assert_eq!(command_of(&it).as_deref(), Some("cd /w && cargo test"), "the command as it ran, not as it's laid out ({cmd:?})");
        let edit = item("e", axel_store::ItemKind::ToolCall, serde_json::json!({ "title": "t", "raw_input": { "file_path": "/w/a.rs" } }));
        assert!(!describe_call(&edit, None, 100, &no_links()).contains("copy?item"), "only commands");
        assert_eq!(command_of(&edit), None);
        // A narrow pane still fits the header's details, copy link included.
        let narrow = plain(&describe_call(&it, Some(1_400), 40, &no_links()));
        assert!(narrow.lines().all(|l| unicode_width::UnicodeWidthStr::width(l) == 40), "{narrow}");
    }

    #[test]
    fn newer_languages_are_highlighted_too() {
        let samples = [
            ("Sources/Sonata/Panel.swift", "struct PanelRoot: View {\n    let model: SonataModel\n}"),
            ("web/app/page.ts", "export const x: number = 1;"),
            ("web/app/page.tsx", "export default function Page() { return <div />; }"),
            ("Cargo.toml", "[package]\nname = \"axel\""),
            ("app/Main.kt", "fun main() { val x = 1 }"),
            ("Dockerfile", "FROM rust:1.90\nRUN cargo build"),
            ("src/main.zig", "const std = @import(\"std\");"),
            ("api/v1.proto", "syntax = \"proto3\";\nmessage A { string b = 1; }"),
            ("schema.graphql", "type Query { a: String }"),
            ("infra/main.tf", "resource \"aws_s3_bucket\" \"b\" { bucket = \"x\" }"),
            // And the ones syntect always had still are.
            ("src/main.rs", "fn main() {}"),
        ];
        for (path, code) in samples {
            assert!(syntax_for(Lang::File(path)).is_some(), "{path}: no syntax");
            let coloured = colour_lines(code, Lang::File(path)).iter().flatten().filter(|(_, c)| c.is_some()).count();
            assert!(coloured > 0, "{path}: nothing coloured");
        }
    }

    #[test]
    fn each_call_is_bands_of_slate() {
        let sec = |text: &str, band: Band| Section { text: text.into(), band };
        let banded = frame(&[sec("▸ cargo test   ✓\r\n", Band::Header), sec("$ ls\r\n", Band::Body), sec("🤖 \x1b[32mdone\x1b[0m\r\n\tx\r\n", Band::Result)], 20);
        let rows: Vec<&str> = banded.split("\r\n").filter(|r| !r.is_empty()).collect();
        let text: Vec<String> = rows.iter().map(|r| plain(r)).collect();
        use unicode_width::UnicodeWidthStr;
        assert!(text.iter().all(|r| r.width() == 20), "every row the width of the pane, wide characters and tabs too: {text:#?}");
        assert_eq!(
            text.iter().map(|r| r.trim_end()).collect::<Vec<_>>(),
            vec!["", "  ▸ cargo test   ✓", "", "", "  $ ls", "", "", "  🤖 done", "      x", ""],
            "a blank line above and below each band's text, two columns in, no border"
        );
        let shade = |row: usize| rows[row].starts_with(&format!("\x1b[{}m", [Band::Header, Band::Body, Band::Result][row / 3].bg()));
        assert!((0..9).all(shade), "each band on its own slate, the blank lines too: {rows:#?}");
        assert!(!rows.iter().any(|r| r.contains(['│', '┌', '▐'])), "no border");
        assert!(rows[7].contains(&format!("{RESET}\x1b[{}m", Band::Result.bg())), "a reset inside a line puts the band's shade back: {:?}", rows[7]);
        let shades = [Band::Header.bg(), Band::Body.bg(), Band::Result.bg()];
        assert!(shades[0] != shades[1] && shades[1] != shades[2] && shades[0] != shades[2], "three different shades");
        // Too long a line is cut, its link closed, rather than push the border out.
        let (cut, used) = fit(&format!("{}{}", axel_tui::util::hyperlink("file:///a", "aaaaaaaaaa"), "b"), 6);
        assert_eq!((plain(&cut).as_str(), used), ("aaaaa…", 6));
        assert!(cut.ends_with("\x1b]8;;\x1b\\"), "{cut:?}");
    }

    #[test]
    fn wrapped_text_hangs_under_its_text() {
        let cells = |s: &str| -> Vec<Cell> { s.chars().map(|c| (c, None)).collect() };
        let text = |ls: Vec<Vec<Cell>>| -> Vec<String> { ls.iter().map(|l| l.iter().map(|(c, _)| *c).collect::<String>().trim_end().to_string()).collect() };
        assert_eq!(text(hanging(&cells("- one two three four five six"), 14)), vec!["- one two", "  three four", "  five six"]);
        assert_eq!(text(hanging(&cells("12. one two three four"), 16)), vec!["12. one two", "    three four"]);
        assert_eq!(text(hanging(&cells("plain words that wrap here"), 12)), vec!["plain words", "that wrap", "here"]);
        // Trailing spaces don't wrap onto a line of their own.
        let padded = emit(&[cells(&format!("remote: visit:{}", " ".repeat(30)))], 20, "");
        assert_eq!(plain(&padded), "remote: visit:\n");
    }

    #[test]
    fn a_heredoc_command_breaks_around_its_body() {
        let md = format!("gh pr view 543 --json body > /tmp/543.md && cat >> /tmp/543.md <<'EOF'\n## Also\n{}\nEOF\ngh pr edit 543 --body-file /tmp/543.md >/dev/null && echo ok", "word ".repeat(30).trim());
        let shown: Vec<String> = plain(&command_block(&md, 60)).lines().map(|l| l.trim_end().to_string()).collect();
        assert_eq!(shown[0], "$ gh pr view 543 --json body > /tmp/543.md &&");
        assert_eq!(shown[1], "  cat >> /tmp/543.md <<'EOF'");
        assert!(shown.iter().all(|l| l.chars().count() <= 60), "the body wraps in the block: {shown:#?}");
        let end = shown.iter().position(|l| l == "EOF").expect("the terminator on its own line");
        assert_eq!(&shown[end + 1..end + 3], ["gh pr edit 543 --body-file /tmp/543.md >/dev/null &&", "  echo ok"]);
    }

    #[test]
    fn editor_links_go_to_the_line() {
        assert_eq!(Editor::named(""), Editor::File);
        assert_eq!(Editor::named("vim"), Editor::File);
        assert_eq!(Editor::named("zed --wait"), Editor::Zed);
        assert_eq!(Editor::named("/usr/local/bin/code -w"), Editor::VsCode);
        assert_eq!(Editor::named("cursor"), Editor::Cursor);
        let p = Path::new("/w/my repo/a.rs");
        assert_eq!(Editor::File.url(p, Some(12)), "file:///w/my%20repo/a.rs", "file:// can't say a line");
        assert_eq!(Editor::Zed.url(p, Some(12)), "zed://file/w/my%20repo/a.rs:12");
        assert_eq!(Editor::VsCode.url(p, None), "vscode://file/w/my%20repo/a.rs");
    }

    #[test]
    fn grep_paths_and_lines_link_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates/core/src")).unwrap();
        std::fs::write(root.join("crates/core/src/send.rs"), "fn send() {}\n").unwrap();
        let links = Linker::new(Some(root.to_path_buf()), Editor::Zed);
        let file = root.join("crates/core/src/send.rs");
        let want = |line: usize| format!("zed://file{}:{line}", url_path(&file));

        // `path:line:` from the thread's directory; a missing file isn't linked.
        let out = "crates/core/src/send.rs:222:fn send() {}\ncrates/gone.rs:3:x";
        let view = grep_view(r#"grep -rn "send" crates"#, out, 120, &links).unwrap();
        assert_eq!(hyperlinks(&view), vec![(want(222), "crates/core/src/send.rs:222".into())]);
        assert!(plain(&view).contains("crates/gone.rs:3:x"));

        // `cd DIR && grep`: relative to DIR.
        let view = grep_view(r#"cd crates && grep -rn "send" core"#, "core/src/send.rs:7:fn send() {}", 120, &links).unwrap();
        assert_eq!(hyperlinks(&view), vec![(want(7), "core/src/send.rs:7".into())]);

        // One file searched: the line number links to it, the nested one too.
        let view = grep_view(r#"grep -n "send" crates/core/src/send.rs | grep -n fn"#, "1396:fn send() {}\n5:92-}", 120, &links).unwrap();
        assert_eq!(hyperlinks(&view), vec![(want(1396), "1396".into()), (want(92), "92".into())]);

        // Mostly not grep output (a file printed first): its `path:N:` lines
        // still link, and a bare `12:` in the rest isn't taken for a result.
        let mixed = "fn a() {}\nlet x = 1;\n12: not a hit\n}\ncrates/core/src/send.rs:428:  send(),";
        let view = grep_view(r#"sed -n 1,4p a.rs; grep -n "send" crates/core/src/*.rs"#, mixed, 120, &links).unwrap();
        assert_eq!(hyperlinks(&view), vec![(want(428), "crates/core/src/send.rs:428".into())]);
        assert!(plain(&view).contains("\n12: not a hit"));
    }

    #[test]
    fn a_link_survives_a_wrap() {
        let cells: Vec<Cell> = "aaaa bbbb cccc".chars().map(|c| (c, None)).collect();
        let link = Link { from: 0, to: 14, url: "file:///x".into() };
        let out = emit_linked(&[cells], &[vec![link]], 10, "", None);
        assert_eq!(hyperlinks(&out), vec![("file:///x".into(), "aaaa bbbb ".into()), ("file:///x".into(), "cccc".into())]);
        assert_eq!(out.matches("\x1b]8;;\x1b\\").count(), 2, "closed at the end of each piece");
    }

    #[test]
    fn edited_read_and_located_files_link_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x\n").unwrap();
        let links = Linker::new(Some(dir.path().to_path_buf()), Editor::VsCode);
        let url = |line: &str| format!("vscode://file{}{line}", url_path(&dir.path().join("a.rs")));
        let call = |input: serde_json::Value, extra: serde_json::Value| {
            let mut c = serde_json::json!({ "title": "t", "raw_input": input });
            c.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            describe_call(&item("t", axel_store::ItemKind::ToolCall, c), None, 100, &links)
        };
        let edit = call(serde_json::json!({ "file_path": "a.rs", "old_string": "x", "new_string": "y" }), serde_json::json!({}));
        assert_eq!(hyperlinks(&edit), vec![(url(""), "a.rs".into())]);
        let read = call(serde_json::json!({ "file_path": "a.rs", "offset": 40, "limit": 10 }), serde_json::json!({}));
        assert_eq!(hyperlinks(&read), vec![(url(":40"), "a.rs  (lines 40–50)".into())]);
        let located = call(serde_json::json!({ "pattern": "x" }), serde_json::json!({ "locations": [{ "path": "a.rs" }, { "path": "nope.rs" }] }));
        assert_eq!(hyperlinks(&located), vec![(url(""), "a.rs".into())]);
    }

    fn item(id: &str, kind: axel_store::ItemKind, content: serde_json::Value) -> axel_store::ConversationItem {
        axel_store::ConversationItem { id: id.into(), session_id: "s".into(), thread_id: "t".into(), seq: 0, turn: 1, at: 0, kind, status: Some(axel_store::ItemStatus::Completed), ref_id: None, content }
    }

    #[test]
    fn a_fold_is_its_tool_calls_until_something_else() {
        use axel_store::ItemKind::*;
        let items = vec![
            item("a", AssistantMessage, serde_json::json!({ "text": "on it" })),
            item("t1", ToolCall, serde_json::json!({})),
            item("t2", ToolCall, serde_json::json!({})),
            item("m", AssistantMessage, serde_json::json!({ "text": "done" })),
            item("t3", ToolCall, serde_json::json!({})),
        ];
        let ids: Vec<&str> = fold_calls(&items, "t1").iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["t1", "t2"]);
        assert!(fold_calls(&items, "nope").is_empty());
    }

    #[test]
    fn a_call_shows_its_command_files_and_output_in_full() {
        let it = item("t1", axel_store::ItemKind::ToolCall, serde_json::json!({
            "title": "Run tests", "kind": "execute",
            "raw_input": { "command": "cargo test -p axel" },
            "locations": [{ "path": "/w/Cargo.toml" }],
            "raw_output": "running 3 tests\ntest result: ok"
        }));
        let s = plain(&describe_call(&it, Some(1_400), 100, &no_links()));
        assert!(s.contains("▸ cargo test  ") && s.contains("1.4s · ✓ · ⧉ copy  \n"), "named by what it runs; a tick, no kind, no empty count: {s}");
        assert_eq!(s.matches("cargo test -p axel").count(), 1, "the command shows once: {s}");
        assert!(s.contains("$ cargo test -p axel"), "{s}");
        assert!(!s.contains("/w/Cargo.toml"), "a command's locations aren't repeated under it: {s}");
        assert!(s.contains("running 3 tests") && s.contains("test result: ok"), "{s}");
    }

    #[test]
    fn each_kind_of_tool_shows_what_it_did() {
        let raw = |input: serde_json::Value| describe_call(&item("t", axel_store::ItemKind::ToolCall, serde_json::json!({ "title": "`x`", "raw_input": input })), None, 100, &no_links());
        let call = |input: serde_json::Value| plain(&raw(input));
        let edit_raw = raw(serde_json::json!({ "file_path": "/w/a.rs", "old_string": "fn f() {\n    let a = 1;\n}", "new_string": "fn f() {\n    let a = 2;\n}" }));
        let edit = plain(&edit_raw);
        assert!(edit.contains("/w/a.rs") && edit.contains("-     let a = 1;") && edit.contains("+     let a = 2;"), "a diff: {edit}");
        assert_eq!(edit.matches("fn f() {").count(), 1, "unchanged lines show once, as context: {edit}");
        assert!(edit_raw.contains(&format!("31;{REMOVED_BG}m- ")) && edit_raw.contains(&format!("32;{ADDED_BG}m+ ")), "red and green, on their backgrounds: {edit_raw:?}");
        assert!(edit_raw.matches("\x1b[").count() > 12, "and the Rust in it highlighted: {edit_raw:?}");
        assert!(edit.contains("▸ x  "), "the title loses its backticks: {edit}");
        let titled = plain(&describe_call(&item("t", axel_store::ItemKind::ToolCall, serde_json::json!({ "title": "Edit `/w/a.rs`" })), None, 100, &no_links()));
        assert!(titled.contains("▸ Edit /w/a.rs  "), "{titled}");
        let read = call(serde_json::json!({ "file_path": "/w/b.rs", "offset": 10, "limit": 20 }));
        assert!(read.contains("/w/b.rs  (lines 10–30)"), "{read}");
        let search = call(serde_json::json!({ "pattern": "fn main", "path": "/w", "glob": "*.rs" }));
        assert!(search.contains("/fn main/  /w  *.rs"), "{search}");
        let cmd = call(serde_json::json!({ "command": "ls", "description": "List the files" }));
        assert!(cmd.contains("List the files") && cmd.contains("$ ls"), "{cmd}");
        let write = call(serde_json::json!({ "file_path": "/w/c.md", "content": "one\ntwo" }));
        assert!(write.contains("/w/c.md  (2 lines)"), "{write}");
        assert_eq!(output_text(Some(&serde_json::json!([{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }]))), "a\nb");
        assert_eq!(strip_line_numbers("     1→fn main() {\n     2→}"), Some("fn main() {\n}".into()), "a read's line numbers go");
        assert_eq!(strip_line_numbers("just text"), None);
        // A line diff keeps what didn't change.
        use Change::*;
        assert_eq!(diff_lines(&["a", "b", "c"], &["a", "x", "c", "d"]), vec![(Same, 0), (Removed, 1), (Added, 1), (Same, 2), (Added, 3)]);
        assert_eq!(diff_lines(&[], &["a"]), vec![(Added, 0)]);
        // An output an older host stored as cut JSON reads as text again.
        assert_eq!(output_text(Some(&serde_json::json!("\"    })\\n    .map(|a| a),\\n    \\\"x\\\"…"))), "    })\n    .map(|a| a),\n    \"x\"…");
        assert_eq!(output_text(Some(&serde_json::json!("[{\"type\":\"text\",\"text\":\"fn main() {\\n}\"},{\"type\":\"te…"))), "fn main() {\n}…");
        assert_eq!(output_text(Some(&serde_json::json!("plain output"))), "plain output", "ordinary text untouched");
        // Search results, ripgrep-style.
        assert_eq!(parse_grep_line("1396:const X: u8 = 1;"), Some(GrepLine::Hit { path: None, outer: None, line: "1396", sep: ':', matched: true, text: "const X: u8 = 1;" }));
        assert_eq!(parse_grep_line("1395-"), Some(GrepLine::Hit { path: None, outer: None, line: "1395", sep: '-', matched: false, text: "" }));
        assert_eq!(parse_grep_line("crates/core/src/client/send.rs:222:    let store = x();"), Some(GrepLine::Hit { path: Some("crates/core/src/client/send.rs"), outer: None, line: "222", sep: ':', matched: true, text: "    let store = x();" }));
        assert_eq!(parse_grep_line("src/a-b.rs-7-ctx"), Some(GrepLine::Hit { path: Some("src/a-b.rs"), outer: None, line: "7", sep: '-', matched: false, text: "ctx" }), "a dash in the path");
        assert_eq!(parse_grep_line("--"), Some(GrepLine::Gap));
        assert_eq!(parse_grep_line("Compiling pay v0.1"), None);
        assert_eq!(grep_terms(r#"grep -n "is_enabled\|pub fn" a.rs"#), vec!["is_enabled", "pub fn"]);
        assert!(grep_terms(r#"grep -n "fo.*o" a.rs"#).is_empty(), "a real regex isn't marked");
        let view = grep_view(r#"grep -n "TIMEOUT" -B1 crates/cli/src/commands/mod.rs"#, "1395-\n1396:const TIMEOUT: u64 = 30;", 120, &no_links()).unwrap();
        assert!(plain(&view).contains("1396:const TIMEOUT: u64 = 30;"), "{view}");
        assert!(view.contains("\x1b[32m1396") && view.contains("\x1b[1;31mTIMEOUT"), "line number green, the term red: {view:?}");
        assert!(view.contains("\x1b[35m") || !view.contains("crates/"), "no path on single-file results");
        let multi = grep_view(r#"grep -rn "default_path()" crates"#, "crates/a.rs:9:    let s = default_path();", 120, &no_links()).unwrap();
        assert!(multi.contains("\x1b[35mcrates/a.rs"), "the path magenta: {multi:?}");
        assert_eq!(grep_view("cargo build", "1:x", 100, &no_links()), None, "not a search");
        // `grep -n … | grep -n …` numbers lines twice: `5:92-    fn id(&self)`.
        assert_eq!(
            parse_grep_line("5:92-    fn id(&self) -> &'static str;"),
            Some(GrepLine::Hit { path: None, outer: Some("5"), line: "92", sep: '-', matched: true, text: "    fn id(&self) -> &'static str;" }),
            "the second grep's `:` makes it a match"
        );
        assert_eq!(parse_grep_line("12:    3-2 = 1"), Some(GrepLine::Hit { path: None, outer: None, line: "12", sep: ':', matched: true, text: "    3-2 = 1" }), "code that starts with spaces isn't a second number");
        // Several files of one language: the results without a path are in it.
        let cmd = r#"grep -n "pub trait RemoteProvider" -A3 crates/core/src/remote/mod.rs; grep -n "pub trait SigningBackend" -A40 crates/core/src/backend/mod.rs | grep -n "fn ""#;
        assert_eq!(file_searched(cmd), Some("crates/core/src/remote/mod.rs"));
        assert_eq!(file_searched("grep -n x a.rs b.py"), None, "two languages: no guess");
        let nested = grep_view(cmd, "190:pub trait RemoteProvider: SigningBackend {\n5:92-    fn id(&self) -> &'static str;", 120, &no_links()).unwrap();
        assert_eq!(
            plain(&nested),
            "190:pub trait RemoteProvider: SigningBackend {\n\n 92:    fn id(&self) -> &'static str;\n",
            "the piped grep's own number dropped, numbers right-aligned, a gap where the next search starts"
        );
        let wrapped = grep_view(r#"grep -n "x" a.rs"#, "191-    /// The credentials to collect, in prompt order", 30, &no_links()).unwrap();
        assert_eq!(plain(&wrapped), "191-    /// The credentials\n        to collect, in prompt\n        order\n", "a wrapped result hangs under its text");
        assert_eq!(only_file(cmd), None, "two files searched: a bare number can't say which");
        assert_eq!(only_file("grep -n x crates/a.rs | grep -n y"), Some("crates/a.rs"));
        assert_eq!(only_file("grep -n x a.rs; sed -n 1,9p a.rs"), Some("a.rs"), "the same file twice is one");
        assert!(nested.contains("\x1b[32m92") && nested.contains("\x1b[35mfn"), "the inner number green, the Rust highlighted: {nested:?}");
        // Highlighted text breaks between words too.
        let cells: Vec<(char, ())> = "with --account kept as a visible alias".chars().map(|c| (c, ())).collect();
        let pieces: Vec<String> = word_chunks(&cells, 16).iter().map(|p| p.iter().map(|(c, _)| *c).collect()).collect();
        assert_eq!(pieces, vec!["with --account ", "kept as a ", "visible alias"]);
        // A heredoc writing Markdown: headings and bold come out bold.
        let md = command_block("cat >> /tmp/543.md <<'EOF'\n## Also in this PR\n- **bold** and `code`\nEOF", 100);
        assert!(md.contains("\x1b[1m## Also in this PR") || md.contains("\x1b[1mAlso in this PR") || md.matches("\x1b[1m").count() >= 2, "{md:?}");
        assert!(md.contains("\x1b[36m"), "inline code is cyan: {md:?}");
        // A command that only prints one file: its output is that file.
        assert_eq!(file_printed_by("sed -n 15,45p crates/cli/src/commands/help.rs"), Some("crates/cli/src/commands/help.rs"));
        assert_eq!(file_printed_by("cd /w && cat src/main.rs | head -40"), Some("src/main.rs"));
        assert_eq!(file_printed_by("head -n 20 'a b/x.py'"), None, "a quoted path with a space splits: not trusted");
        assert_eq!(file_printed_by("sed -n 990,1110p crates/mcp/src/tools/curl.rs; sed -n 440,520p crates/cli/src/commands/account/new.rs"), Some("crates/mcp/src/tools/curl.rs"), "reads of one language, chained");
        assert_eq!(file_printed_by("sed -n 1,9p a.rs; sed -n 1,9p b.py"), None, "two languages");
        assert_eq!(file_printed_by("cat a.rs; cargo build"), None, "not only reads");
        assert_eq!(file_printed_by("grep -n foo a.rs"), None, "a search, not the file");
        assert_eq!(file_printed_by("cat a.rs | grep foo"), None, "filtered");
        assert_eq!(file_printed_by("cat a.rs b.rs"), None);
        // A heredoc's body is the language of what reads it.
        let py = split_heredoc("$ cd /w && python3 - <<'EOF'\nprint('hi')\nEOF\ncargo build").unwrap();
        assert_eq!((py.head, py.body, py.rest), ("$ cd /w && python3 - <<'EOF'", "print('hi')", "EOF\ncargo build"));
        assert!(matches!(py.lang, Lang::Named("python")));
        assert!(matches!(split_heredoc("$ cat > src/main.rs <<EOF\nfn main() {}\nEOF").unwrap().lang, Lang::File("src/main.rs")));
        assert!(matches!(split_heredoc("$ gh pr edit 1 --body-file - <<'MD'\n# Title\nMD").unwrap().lang, Lang::Named("txt")), "unknown readers stay plain");
        assert_eq!(split_heredoc("$ echo 'a << b'"), None, "no newline, no heredoc");
        let shown = plain(&command_block("python3 - <<'EOF'\nimport os\nEOF", 100));
        assert_eq!(shown, "$ python3 - <<'EOF'\nimport os\nEOF\n", "just the command: its band brings the margins");
        let long = plain(&command_block(&format!("echo {}", "x".repeat(40)), 20));
        assert!(long.lines().count() >= 3 && long.lines().all(|l| l.chars().count() <= 20), "too long a word breaks where it must: {long}");
        assert_eq!(without_harness_notes("    Finished `dev`\nShell cwd was reset to /Users/x/pay\n"), "    Finished `dev`");
        assert_eq!(without_harness_notes("Shell cwd was reset to /x"), "", "nothing printed");
        // Long lines wrap with their indent kept.
        let w = plain(&wrap_plain(&"x".repeat(25), 14, INDENT, ""));
        assert_eq!(w, "xxxxxxxxxxxxxx\nxxxxxxxxxxx\n");
        let words = plain(&wrap_plain("please do --from by default and keep the alias", 24, "", ""));
        assert_eq!(words, "please do --from by \ndefault and keep the \nalias\n".replace(" \n", "\n"), "prose breaks between words");
        assert_eq!(duration(81_000), "1m21s");
        assert_eq!(duration(400), "0.4s");
    }

    #[test]
    fn the_applet_hands_each_url_to_axel_quoted() {
        let src = applet_source(Path::new("/Users/me/.local/bin/axel"), Path::new("/Users/me/.axel/logs/links.log"));
        assert!(src.starts_with("on open location theURL\n"));
        assert!(src.contains("quoted form of \"/Users/me/.local/bin/axel\" & \" open-url \" & quoted form of theURL"), "{src}");
        assert!(src.contains("\" >> \" & quoted form of \"/Users/me/.axel/logs/links.log\" & \" 2>&1 &\""), "a failed click leaves a log: {src}");
        let odd = applet_source(Path::new("/tmp/a \"b\"/axel"), Path::new("/tmp/l"));
        assert!(odd.contains("quoted form of \"/tmp/a \\\"b\\\"/axel\""), "quotes in the path are escaped: {odd}");
    }
}

