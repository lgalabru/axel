//! `axel thread diff`: what one turn changed, from its checkpoints.
//!
//! The turn envelope carries the two checkpoint commits and the file list;
//! the diff itself is read from the workspace's repository here (fetched from
//! the host that ran the turn when it isn't here yet).  As a command it
//! prints the file list (`--stat`) or the list and the patch.  As the
//! floating pane a click on a change line opens (`--pane`), it is the diff
//! pane of `axel_tui::diff`: read, mark lines, write notes and send them to
//! the agent as one review, step between turns, undo the turn.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use axel_store::{ChangeStatus, FileChange, Turn, TurnChanges};
use axel_tui::diff::{DiffData, Draft, FileEntry, Outcome, parse_patch, run_diff};

use super::{Ctx, undo};
use crate::paths;

/// A turn of a thread that left checkpoints: the choice `axel thread diff` shows.
pub struct Pick {
    pub turn: Turn,
    pub changes: TurnChanges,
    pub agent: String,
}

/// The turn to show: `turn` of `session` when given (a session id or prefix),
/// else the latest turn that changed a file.
pub fn pick(ctx: &Ctx, thread_id: &str, session: Option<&str>, turn: Option<i64>) -> Result<Pick> {
    let mut turns = ctx.store.turns(thread_id)?;
    if let Some(s) = session {
        turns.retain(|t| t.session_id.starts_with(s));
        if turns.is_empty() {
            bail!("no session {s}… on this thread");
        }
    }
    if let Some(n) = turn {
        turns.retain(|t| t.summary.ordinal == n);
    }
    // Newest last; with a turn asked for, the newest session that has it.
    let chosen = match turn {
        Some(n) => turns.pop().with_context(|| format!("no turn {n} on this thread"))?,
        None => turns
            .into_iter()
            .rev()
            .find(|t| t.summary.changes.as_ref().is_some_and(|c| !c.files.is_empty()))
            .context("no turn on this thread changed a file (or none left checkpoints)")?,
    };
    let agent = agent_name(ctx, &chosen.session_id).unwrap_or_else(|| "agent".into());
    let Some(changes) = chosen.summary.changes.clone() else {
        bail!("turn {} of {agent} left no checkpoints (not a git workspace, or an older build)", chosen.summary.ordinal);
    };
    Ok(Pick { turn: chosen, changes, agent })
}

fn agent_name(ctx: &Ctx, session_id: &str) -> Option<String> {
    let s = ctx.store.get_session(session_id).ok()??;
    ctx.store.agent_by_pubkey(&s.agent_pubkey).ok()?.map(|a| a.name)
}

/// `3 files +42 −7`.
pub fn headline(c: &TurnChanges) -> String {
    let (a, r) = c.totals();
    format!("{} file{} +{a} −{r}", c.files.len(), if c.files.len() == 1 { "" } else { "s" })
}

/// The file list: the agent's own files, then (when there are others) the
/// rest under their own heading.  Plain text, one file per line.
pub fn stat(c: &TurnChanges) -> String {
    let width = c.files.iter().map(|f| label(f).chars().count()).max().unwrap_or(0).min(80);
    let line = |f: &FileChange| {
        let counts = match (f.added, f.removed) {
            (Some(a), Some(r)) => format!("+{a} −{r}"),
            _ => "binary".to_string(),
        };
        format!("{} {:<width$}  {counts}\n", f.status.letter(), label(f))
    };
    let split = c.files.iter().any(|f| f.authored) && c.files.iter().any(|f| !f.authored);
    let mut out = String::new();
    if split {
        c.files.iter().filter(|f| f.authored).for_each(|f| out.push_str(&line(f)));
        out.push_str("── also changed during this turn ──\n");
        c.files.iter().filter(|f| !f.authored).for_each(|f| out.push_str(&line(f)));
    } else {
        c.files.iter().for_each(|f| out.push_str(&line(f)));
    }
    if c.truncated {
        out.push_str(&format!("… and more: only the first {} files are listed\n", TurnChanges::MAX_FILES));
    }
    if c.between_turns {
        out.push_str("(includes changes made between this turn and the previous one)\n");
    }
    out
}

fn label(f: &FileChange) -> String {
    match (&f.old_path, f.status) {
        (Some(old), ChangeStatus::Renamed) => format!("{old} → {}", f.path),
        _ => f.path.clone(),
    }
}

/// The workspace repository holds both checkpoint commits.
fn has_checkpoints(dir: &Path, c: &TurnChanges) -> bool {
    [&c.before, &c.after].iter().all(|oid| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["cat-file", "-e", &format!("{oid}^{{commit}}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// The directory whose repository holds the turn's checkpoints: the thread's
/// workspace here, after fetching them from the host that ran the turn when
/// they aren't here yet.  `Err` says where they are.
fn repo_with_checkpoints(ctx: &Ctx, thread_id: &str, turn: &Turn, c: &TurnChanges) -> Result<PathBuf> {
    let dir = super::links::thread_dir(ctx, thread_id).context("this machine has no checkout of the thread's project")?;
    if has_checkpoints(&dir, c) {
        return Ok(dir);
    }
    let session = ctx.store.get_session(&turn.session_id)?;
    if let Some(host) = session.as_ref().and_then(|s| super::host::host_of(ctx, s).ok().flatten()) {
        super::host::fetch_checkpoints(&host, &dir, thread_id).with_context(|| format!("fetching the checkpoints from {}", host.label))?;
        if has_checkpoints(&dir, c) {
            return Ok(dir);
        }
    }
    let label = session.map(|s| s.environment_label).unwrap_or_else(|| "another machine".into());
    bail!("the diff lives on {label}: this machine's checkout has no checkpoints for this turn")
}

/// `git diff before after` in `dir`, coloured when `color`.
fn patch(dir: &Path, c: &TurnChanges, color: bool) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["diff", if color { "--color=always" } else { "--color=never" }, "-M", "--no-ext-diff", &c.before, &c.after])
        .output()
        .context("running git diff")?;
    if !out.status.success() {
        bail!("git diff: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `axel thread diff THREAD [--session S] [--turn N] [--stat] [--pane]`.
pub fn run(thread: &str, session: Option<&str>, turn: Option<i64>, stat_only: bool, pane: bool) -> Result<()> {
    let ctx = Ctx::open()?;
    let t = super::thread::resolve(&ctx, thread)?;
    if pane {
        let res = run_pane(&ctx, &t, session, turn);
        super::ui::close_own_layer(&ctx);
        return res;
    }
    let p = pick(&ctx, &t.id, session, turn)?;
    println!("{} · turn {} · {}", p.agent, p.turn.summary.ordinal, headline(&p.changes));
    print!("{}", stat(&p.changes));
    if stat_only || p.changes.files.is_empty() {
        return Ok(());
    }
    let dir = repo_with_checkpoints(&ctx, &t.id, &p.turn, &p.changes)?;
    println!();
    let color = std::io::IsTerminal::is_terminal(&std::io::stdout());
    print!("{}", patch(&dir, &p.changes, color)?);
    Ok(())
}

/// Read bounded immutable blobs. Highlighting is optional: missing, binary,
/// oversized or unusually long-lined sources keep the plain patch display.
fn snapshot_syntax(dir: &Path, revision: &str, path: &str, budget: &mut usize) -> Option<Vec<Vec<axel_tui::diff::SyntaxSpan>>> {
    let object = format!("{revision}:{path}");
    let size = Command::new("git").arg("-C").arg(dir).args(["cat-file", "-s", &object]).stderr(Stdio::null()).output().ok()?;
    if !size.status.success() { return None }
    let size: usize = std::str::from_utf8(&size.stdout).ok()?.trim().parse().ok()?;
    if size > 512 * 1024 || size > *budget { return None }
    let output = Command::new("git").arg("-C").arg(dir).args(["cat-file", "blob", &object]).stderr(Stdio::null()).output().ok()?;
    if !output.status.success() { return None }
    *budget = budget.saturating_sub(output.stdout.len());
    let source = std::str::from_utf8(&output.stdout).ok()?;
    if source.contains('\0') || source.lines().any(|line| line.len() > 8192) { return None }
    Some(super::links::diff_syntax(source, path))
}

fn checkpoint_syntax(dir: &Path, changes: &TurnChanges, patches: &[axel_tui::diff::FilePatch]) -> std::collections::BTreeMap<String, Vec<Vec<axel_tui::diff::SyntaxSpan>>> {
    use axel_tui::diff::LineKind;
    let mut budget = 4 * 1024 * 1024;
    patches.iter().map(|patch| {
        let original = changes.files.iter().find(|f| f.path == patch.path).and_then(|f| f.old_path.as_deref()).unwrap_or(&patch.path);
        let old = snapshot_syntax(dir, &changes.before, original, &mut budget).unwrap_or_default();
        let new = snapshot_syntax(dir, &changes.after, &patch.path, &mut budget).unwrap_or_default();
        let lines = patch.lines.iter().map(|line| {
            let (source, number) = if line.kind == LineKind::Del { (&old, line.old) } else { (&new, line.new) };
            number.and_then(|n| n.checked_sub(1)).and_then(|n| source.get(n)).cloned().unwrap_or_default()
        }).collect();
        (patch.path.clone(), lines)
    }).collect()
}

/// The pane: show the turn, act on what the pane asks, show again.
fn run_pane(ctx: &Ctx, t: &axel_store::Thread, session: Option<&str>, turn: Option<i64>) -> Result<()> {
    let mut p = pick(ctx, &t.id, session, turn)?;
    let mut note: Option<String> = None;
    let mut offer_restore = false;
    loop {
        let session_id = p.turn.session_id.clone();
        let ordinal = p.turn.summary.ordinal;
        let dir = repo_with_checkpoints(ctx, &t.id, &p.turn, &p.changes);
        let mut data = DiffData {
            agent: p.agent.clone(),
            turn: ordinal,
            title: t.title.clone(),
            headline: headline(&p.changes),
            files: p.changes.files.iter().map(entry).collect(),
            patches: Vec::new(),
            syntax: Default::default(),
            between_turns: p.changes.between_turns,
            truncated: p.changes.truncated,
            note: note.take(),
            offer_restore,
            can_undo: false,
        };
        match &dir {
            Ok(dir) => {
                if !p.changes.files.is_empty() {
                    data.patches = parse_patch(&patch(dir, &p.changes, false)?);
                    data.syntax = checkpoint_syntax(dir, &p.changes, &data.patches);
                    let blocked = undo::blocked_reason(ctx, &session_id)?;
                    data.can_undo = blocked.is_none();
                    if let Some(why) = blocked
                        && data.note.is_none()
                    {
                        data.note = Some(format!("no undo: {why}"));
                    }
                }
            }
            Err(e) => data.note = Some(format!("{e:#}")),
        }
        let drafts = load_drafts(&session_id, ordinal);
        let (outcome, drafts) = run_diff(data, drafts)?;
        save_drafts(&session_id, ordinal, &drafts)?;
        offer_restore = false;
        match outcome {
            Outcome::Close => return Ok(()),
            Outcome::Turn(delta) => match pick(ctx, &t.id, Some(&session_id), Some(ordinal + delta)) {
                Ok(next) => p = next,
                Err(e) => note = Some(format!("{e:#}")),
            },
            Outcome::Send(text) => {
                note = Some(match deliver(ctx, &t.id, &session_id, &text) {
                    Ok(how) => {
                        save_drafts(&session_id, ordinal, &[])?;
                        format!("review sent to {} ({how})", p.agent)
                    }
                    Err(e) => format!("not sent: {e:#}"),
                });
            }
            Outcome::Undo => {
                let dir = dir?;
                note = Some(match undo::undo(&dir, &t.id, &p.changes)? {
                    undo::UndoResult::Done(done) => {
                        if let Err(e) = undo::notify(ctx, &t.id, &session_id, ordinal, &done.files, false) {
                            eprintln!("axel: telling the agent: {e:#}");
                        }
                        format!("undone: {} back to before turn {ordinal} (snapshot {})", files_phrase(&done.files), short_ref(&done.snapshot_ref))
                    }
                    undo::UndoResult::Conflicts(files) => {
                        offer_restore = true;
                        format!("later work overlaps {}: nothing changed. R restores the turn's files to before it", files_phrase(&files))
                    }
                });
            }
            Outcome::Restore => {
                let dir = dir?;
                let done = undo::restore(&dir, &t.id, &p.changes)?;
                if let Err(e) = undo::notify(ctx, &t.id, &session_id, ordinal, &done.files, true) {
                    eprintln!("axel: telling the agent: {e:#}");
                }
                note = Some(format!("restored: {} as before turn {ordinal} (snapshot {})", files_phrase(&done.files), short_ref(&done.snapshot_ref)));
            }
        }
    }
}

fn entry(f: &FileChange) -> FileEntry {
    FileEntry { path: f.path.clone(), label: label(f), status: f.status.letter(), added: f.added, removed: f.removed, authored: f.authored }
}

fn files_phrase(files: &[String]) -> String {
    match files {
        [] => "nothing".into(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, rest @ ..] => format!("{a} and {} more", rest.len()),
    }
}

/// `undo/3` from `refs/axel/checkpoints/<thread>/undo/3`.
fn short_ref(name: &str) -> String {
    name.rsplit('/').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/")
}

/// Get a review in front of the session's agent: into its live managed
/// session, else by summoning it back onto the thread (it resumes its
/// conversation).  Says how it went.
fn deliver(ctx: &Ctx, thread_id: &str, session_id: &str, text: &str) -> Result<&'static str> {
    let session = ctx.store.get_session(session_id)?.context("no such session")?;
    let live = session.ended_at.is_none() && session.runtime == axel_store::SessionRuntime::Managed;
    if live {
        let cmd = axel_tui::SendCommand::Prompt { text: text.to_string(), attachments: vec![], steer: false };
        match super::ui::send_to_session(ctx, session_id, cmd)? {
            axel_tui::SendReply::Error { message } => bail!("{message}"),
            axel_tui::SendReply::Queued { .. } => return Ok("queued for its next turn"),
            axel_tui::SendReply::Steering { .. } => return Ok("into the running turn"),
            _ => return Ok("as a prompt"),
        }
    }
    // SAFETY: single-threaded CLI; set before the send reads it.
    unsafe { std::env::set_var("AXEL_THREAD", thread_id) };
    let report = super::inbox::send_message(ctx, std::slice::from_ref(&session.agent_pubkey), &[], text, axel_mcp::MessageKind::Request)?;
    if let Some(d) = report.deliveries.iter().find(|d| matches!(d.outcome, axel_mcp::Outcome::Failed { .. })) {
        bail!("{}", d.outcome.describe());
    }
    Ok("summoned back onto the thread")
}

// ---------------------------------------------------------------- drafts

/// Where a turn's unsent notes wait: `~/.axel/reviews/<session>-<turn>.json`.
fn drafts_path(session_id: &str, turn: i64) -> PathBuf {
    paths::home().join("reviews").join(format!("{session_id}-{turn}.json"))
}

fn load_drafts(session_id: &str, turn: i64) -> Vec<Draft> {
    std::fs::read_to_string(drafts_path(session_id, turn)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_drafts(session_id: &str, turn: i64, drafts: &[Draft]) -> Result<()> {
    let path = drafts_path(session_id, turn);
    if drafts.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, serde_json::to_vec_pretty(drafts)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, status: ChangeStatus, added: Option<i64>, removed: Option<i64>, authored: bool) -> FileChange {
        FileChange { path: path.into(), old_path: None, status, added, removed, authored }
    }

    #[test]
    fn syntax_uses_both_checkpoint_blobs_and_keeps_multiline_parser_state() {
        use axel_tui::diff::{LineKind, SyntaxTone};
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git").arg("-C").arg(dir.path()).args(args).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.path().join("old.rs"), "/*\nlet still_a_comment = 1;\n*/\nfn retry() { old(); }\n").unwrap();
        git(&["add", "."]);
        let before = git(&["write-tree"]);
        std::fs::remove_file(dir.path().join("old.rs")).unwrap();
        std::fs::write(dir.path().join("new.rs"), "/*\nlet still_a_comment = 1;\n*/\nfn retry() { new(); }\n").unwrap();
        git(&["add", "-A"]);
        let after = git(&["write-tree"]);
        let patches = parse_patch(&git(&["diff", "--no-ext-diff", "-M30%", &before, &after]));
        let changes = TurnChanges { before, after, files: vec![FileChange {
            path: "new.rs".into(), old_path: Some("old.rs".into()), status: ChangeStatus::Renamed, ..Default::default()
        }], ..Default::default() };
        // The working tree has already moved on from the turn being reviewed.
        std::fs::write(dir.path().join("new.rs"), "not the reviewed source").unwrap();
        let syntax = checkpoint_syntax(dir.path(), &changes, &patches);
        let patch = patches.iter().find(|p| p.path == "new.rs").unwrap();
        for (i, line) in patch.lines.iter().enumerate().filter(|(_, l)| !matches!(l.kind, LineKind::Hunk | LineKind::Meta)) {
            let spans = &syntax["new.rs"][i];
            assert_eq!(spans.iter().map(|s| s.text.as_str()).collect::<String>(), line.text);
            if line.text.contains("still_a_comment") { assert!(spans.iter().all(|s| s.tone == SyntaxTone::Muted)); }
            if line.kind == LineKind::Add { assert!(spans.iter().any(|s| s.tone == SyntaxTone::Keyword)); }
        }
        assert!(snapshot_syntax(dir.path(), &changes.after, "new.rs", &mut 0).is_none(), "plain fallback when the display budget is exhausted");
    }

    #[test]
    fn stat_lists_the_agents_files_first_and_the_rest_apart() {
        let c = TurnChanges {
            before: "a".into(),
            after: "b".into(),
            files: vec![
                file("Cargo.lock", ChangeStatus::Modified, Some(0), Some(1), false),
                file("src/view.rs", ChangeStatus::Modified, Some(38), Some(5), true),
                file("img.png", ChangeStatus::Added, None, None, true),
                FileChange { path: "b.rs".into(), old_path: Some("a.rs".into()), status: ChangeStatus::Renamed, added: Some(1), removed: Some(0), authored: true },
            ],
            truncated: true,
            between_turns: true,
        };
        assert_eq!(headline(&c), "4 files +39 −6");
        let s = stat(&c);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "M src/view.rs  +38 −5");
        assert_eq!(lines[1], "A img.png      binary");
        assert_eq!(lines[2], "R a.rs → b.rs  +1 −0");
        assert_eq!(lines[3], "── also changed during this turn ──");
        assert_eq!(lines[4], "M Cargo.lock   +0 −1");
        assert!(lines[5].starts_with("… and more"));
        assert!(lines[6].starts_with("(includes changes made between"));

        let own = TurnChanges { files: vec![file("x.rs", ChangeStatus::Deleted, Some(0), Some(9), false)], ..Default::default() };
        assert_eq!(stat(&own), "D x.rs  +0 −9\n", "all by one hand: no heading");
        assert_eq!(headline(&own), "1 file +0 −9");
        assert_eq!(short_ref("refs/axel/checkpoints/T/undo/3"), "undo/3");
        assert_eq!(files_phrase(&["a".into(), "b".into(), "c".into()]), "a and 2 more");
    }
}
