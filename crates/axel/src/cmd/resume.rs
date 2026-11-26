//! Coming back after Rex restarts (OSC 7577, the Program Resume Protocol).
//!
//! Each long-running Axel program in a Rex pane tells Rex the command that
//! brings it back: the thread's stream, composer and usage panes run again
//! as they are, and an agent's session, managed or in a pane, comes back as
//! `axel session resume SESSION`, which continues its harness conversation.
//! The floating launcher and its popups don't report: they are summoned, not
//! kept.  Reports go out only where Rex gave a token; elsewhere they are
//! no-ops.

use std::path::{Path, PathBuf};

use axel_rex::resume::{Action, report};

/// Report that this process comes back as `axel ARGS` in `cwd`.
pub fn come_back_as(args: &[&str], cwd: Option<&Path>) {
    let mut argv = vec![axel_program()];
    argv.extend(args.iter().map(|a| a.to_string()));
    // Best effort: a terminal that can't take the report just won't bring
    // this pane back, which is how it was before.
    let _ = report(Action::Resume { argv: &argv, cwd });
}

/// Report that this process shouldn't come back: the user ended it.
pub fn forget() {
    // Best effort, as above: the record also goes when this process exits.
    let _ = report(Action::Clear);
}

/// How a restarted shell finds this binary: `axel` when that is what PATH
/// gives, so an upgrade that moves the binary still resumes; else the path
/// it runs from.
fn axel_program() -> String {
    let me = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok());
    let on_path = super::launch::which("axel").and_then(|p| std::fs::canonicalize(p).ok());
    match (me, on_path) {
        (Some(me), Some(found)) if me == found => "axel".to_string(),
        (Some(me), _) => me.to_string_lossy().into_owned(),
        (None, _) => "axel".to_string(),
    }
}

/// The token, for an Axel program this process execs in its own pane, so
/// that one can report in turn.  Harnesses never get it.
pub fn token_for_axel(program: &str) -> Option<String> {
    let me = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok())?;
    let target = std::fs::canonicalize(PathBuf::from(program)).ok()?;
    (me == target).then(axel_rex::resume::token).flatten()
}
