//! Turn checkpoints: a hidden commit of the working tree before and after
//! each turn, so what a turn changed can be shown, reviewed and undone.
//!
//! A snapshot commits the tree (tracked and untracked files, not ignored
//! ones) through a temporary index on top of HEAD, unsigned, without touching
//! the user's index, branches or HEAD; a clean tree is HEAD itself.  Each one
//! is kept alive under `refs/axel/checkpoints/<thread>/<session>/<ordinal>/
//! {before,after}`: nothing lists or pushes `refs/axel/*`.  Only the commit
//! ids and the file list travel in the turn envelope; the diff stays here.
//!
//! Every git call has a time budget, so a huge untracked file can delay a
//! turn by a few seconds at most and never hang it.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axel_store::{ChangeStatus, FileChange, TurnChanges};

/// Where checkpoints live in the repository.
pub const REFS: &str = "refs/axel/checkpoints";

/// The most a checkpoint may hold up a turn.
pub const BUDGET: Duration = Duration::from_secs(10);

/// Past this, a checkpoint is slow enough to tell the owner about.
pub const SLOW: Duration = Duration::from_secs(2);

/// A snapshot of the working tree: its commit, and the tree it points at (two
/// snapshots with one tree have the same content, whatever their parents).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub commit: String,
    pub tree: String,
}

/// Which end of a turn a checkpoint marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Before,
    After,
}

impl Which {
    fn as_str(self) -> &'static str {
        match self {
            Which::Before => "before",
            Which::After => "after",
        }
    }
}

/// `refs/axel/checkpoints/<thread>/<session>/<ordinal>/<before|after>`.
pub fn ref_name(thread_id: &str, session_id: &str, ordinal: i64, which: Which) -> String {
    format!("{REFS}/{thread_id}/{session_id}/{ordinal}/{}", which.as_str())
}

/// Snapshot `dir`'s tree and keep it under `ref_name`.  `None` when there is
/// nothing to checkpoint: `dir` isn't in a git repository, or the repository
/// has no commit yet.  An error past `budget` leaves no ref behind.  With
/// `same_as`, a tree equal to that snapshot's is that snapshot (so a turn
/// that changed nothing has `after == before`).
pub fn capture(dir: &Path, ref_name: &str, budget: Duration, same_as: Option<&Snapshot>) -> Result<Option<Snapshot>> {
    let deadline = Instant::now() + budget;
    let Some(mut snap) = take(dir, deadline)? else { return Ok(None) };
    if let Some(prev) = same_as.filter(|p| p.tree == snap.tree) {
        snap = prev.clone();
    }
    git(dir, &["update-ref", ref_name, &snap.commit], &[], deadline)?;
    Ok(Some(snap))
}

/// A snapshot of `dir`'s tree, kept by nothing (see [`capture`]).
pub fn take(dir: &Path, deadline: Instant) -> Result<Option<Snapshot>> {
    if !is_repo_with_head(dir, deadline) {
        return Ok(None);
    }
    let commit = snapshot_commit_until(dir, "axel: turn checkpoint", deadline)?;
    let tree = git(dir, &["rev-parse", &format!("{commit}^{{tree}}")], &[], deadline)?;
    Ok(Some(Snapshot { commit, tree }))
}

/// `dir` is inside a git work tree that has a commit.
fn is_repo_with_head(dir: &Path, deadline: Instant) -> bool {
    git(dir, &["rev-parse", "--is-inside-work-tree"], &[], deadline).is_ok_and(|s| s == "true")
        && git(dir, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"], &[], deadline).is_ok()
}

/// A commit of `dir`'s working tree on top of HEAD (tracked and untracked
/// files, not ignored ones), or HEAD itself when nothing changed.  The
/// user's index and branches are untouched.
pub fn snapshot_commit(dir: &Path, message: &str) -> Result<String> {
    snapshot_commit_until(dir, message, Instant::now() + Duration::from_secs(600))
}

fn snapshot_commit_until(dir: &Path, message: &str, deadline: Instant) -> Result<String> {
    let head = git(dir, &["rev-parse", "HEAD"], &[], deadline)?;
    let index = std::env::temp_dir().join(format!("axel-index-{}-{}", std::process::id(), axel_store::new_id()));
    let env: [(&str, &OsStr); 1] = [("GIT_INDEX_FILE", index.as_os_str())];
    let result = (|| {
        git(dir, &["read-tree", "HEAD"], &env, deadline)?;
        git(dir, &["add", "-A", "."], &env, deadline)?;
        let tree = git(dir, &["write-tree"], &env, deadline)?;
        if tree == git(dir, &["rev-parse", "HEAD^{tree}"], &[], deadline)? {
            return Ok(head.clone());
        }
        // Never signed: a snapshot must not wait on a signing agent's prompt.
        git(dir, &["commit-tree", "--no-gpg-sign", &tree, "-p", &head, "-m", message], &env, deadline)
    })();
    let _ = std::fs::remove_file(&index);
    let _ = std::fs::remove_file(index.with_extension("lock"));
    result
}

/// What differs between two snapshots: each file with its status and line
/// counts, `authored` when one of `authored` (absolute paths the session's
/// edit tools named) is that file.  At most [`TurnChanges::MAX_FILES`] files.
pub fn changes(dir: &Path, before: &Snapshot, after: &Snapshot, authored: &[PathBuf]) -> Result<TurnChanges> {
    let mut out = TurnChanges { before: before.commit.clone(), after: after.commit.clone(), ..Default::default() };
    if before.tree == after.tree {
        return Ok(out);
    }
    let deadline = Instant::now() + BUDGET;
    let root = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"], &[], deadline)?);
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let authored: Vec<String> = authored.iter().filter_map(|p| relative(&root, p)).collect();
    let numstat = git(dir, &["diff-tree", "-r", "-M", "-z", "--numstat", &before.tree, &after.tree], &[], deadline)?;
    let status = git(dir, &["diff-tree", "-r", "-M", "-z", "--name-status", &before.tree, &after.tree], &[], deadline)?;
    let mut files = parse(&numstat, &status);
    for f in &mut files {
        f.authored = authored.contains(&f.path);
    }
    out.truncated = files.len() > TurnChanges::MAX_FILES;
    files.truncate(TurnChanges::MAX_FILES);
    out.files = files;
    Ok(out)
}

/// `path` relative to `root`, when it is under it.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.strip_prefix(root).ok().map(|p| p.to_string_lossy().into_owned())
}

/// Join `git diff-tree -z --numstat` and `-z --name-status` output into files,
/// in numstat order.  Numstat: `added\tremoved\tpath` per entry, and for a
/// rename `added\tremoved\t` followed by the old and new paths as entries of
/// their own.  Name-status: the status, then the path (two for a rename).
pub fn parse(numstat: &str, name_status: &str) -> Vec<FileChange> {
    let mut status: HashMap<String, (ChangeStatus, Option<String>)> = HashMap::new();
    let mut toks = name_status.split('\0').filter(|t| !t.is_empty());
    while let Some(st) = toks.next() {
        let kind = ChangeStatus::from_letter(st);
        let Some(first) = toks.next() else { break };
        if matches!(st.chars().next(), Some('R' | 'C')) {
            let Some(second) = toks.next() else { break };
            status.insert(second.to_string(), (kind, Some(first.to_string())));
        } else {
            status.insert(first.to_string(), (kind, None));
        }
    }
    let count = |s: &str| if s == "-" { None } else { s.parse::<i64>().ok() };
    let mut files = Vec::new();
    let mut toks = numstat.split('\0').filter(|t| !t.is_empty()).peekable();
    while let Some(entry) = toks.next() {
        let mut parts = entry.splitn(3, '\t');
        let (Some(a), Some(r), Some(path)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let (added, removed) = (count(a), count(r));
        let (path, old_path) = if path.is_empty() {
            // A rename: the paths follow as entries of their own.
            let (Some(old), Some(new)) = (toks.next(), toks.next()) else { break };
            (new.to_string(), Some(old.to_string()))
        } else {
            (path.to_string(), None)
        };
        let (st, old_from_status) = status.get(&path).cloned().unwrap_or((
            if old_path.is_some() { ChangeStatus::Renamed } else { ChangeStatus::Modified },
            None,
        ));
        files.push(FileChange { path, old_path: old_path.or(old_from_status), status: st, added, removed, authored: false });
    }
    files
}

/// `git -C dir args`, stdout trimmed; killed, with an error, past `deadline`.
pub fn git(dir: &Path, args: &[&str], env: &[(&str, &OsStr)], deadline: Instant) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().context("running git")?;
    // Read both pipes on threads, so a chatty command can't block on a full pipe.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stdout.read_to_end(&mut v);
        v
    });
    let err = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stderr.read_to_end(&mut v);
        v
    });
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git {} took longer than its time budget", args.first().copied().unwrap_or(""));
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if !status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&stderr).trim());
    }
    Ok(String::from_utf8_lossy(&stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["commit", "--quiet", "--allow-empty", "-m", "root"],
        ] {
            git(d.path(), &args, &[], far()).unwrap();
        }
        d
    }

    #[test]
    fn snapshots_carry_uncommitted_and_untracked_files_but_not_ignored_ones() {
        let d = repo();
        let head = git(d.path(), &["rev-parse", "HEAD"], &[], far()).unwrap();
        assert_eq!(snapshot_commit(d.path(), "s").unwrap(), head, "clean tree: HEAD itself");
        std::fs::write(d.path().join(".gitignore"), ".env\n").unwrap();
        std::fs::write(d.path().join(".env"), "SECRET=1").unwrap();
        std::fs::write(d.path().join("new.rs"), "fn main() {}").unwrap();
        let snap = snapshot_commit(d.path(), "s").unwrap();
        assert_ne!(snap, head);
        let files = git(d.path(), &["ls-tree", "--name-only", &snap], &[], far()).unwrap();
        assert!(files.contains("new.rs") && files.contains(".gitignore") && !files.contains(".env"), "{files}");
        assert_eq!(git(d.path(), &["rev-parse", "HEAD"], &[], far()).unwrap(), head, "HEAD untouched");
        assert!(git(d.path(), &["diff", "--cached", "--name-only"], &[], far()).unwrap().is_empty(), "index untouched");
    }

    #[test]
    fn a_turn_is_captured_as_two_refs_and_their_difference() {
        let d = repo();
        std::fs::write(d.path().join("a.rs"), "one\ntwo\n").unwrap();
        std::fs::write(d.path().join("gone.txt"), "bye\n").unwrap();
        std::fs::write(d.path().join("pic.png"), [0u8, 1, 2]).unwrap();
        git(d.path(), &["add", "."], &[], far()).unwrap();
        git(d.path(), &["commit", "--quiet", "-m", "base"], &[], far()).unwrap();
        let before = capture(d.path(), &ref_name("T", "S", 1, Which::Before), BUDGET, None).unwrap().unwrap();
        // The turn: edits a.rs (authored), deletes gone.txt, adds b.rs, changes a binary.
        std::fs::write(d.path().join("a.rs"), "one\n2\nthree\n").unwrap();
        std::fs::remove_file(d.path().join("gone.txt")).unwrap();
        std::fs::write(d.path().join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(d.path().join("pic.png"), [9u8, 9]).unwrap();
        let after = capture(d.path(), &ref_name("T", "S", 1, Which::After), BUDGET, Some(&before)).unwrap().unwrap();
        assert_ne!(before.tree, after.tree);
        let refs = git(d.path(), &["for-each-ref", "--format=%(refname)", REFS], &[], far()).unwrap();
        assert_eq!(refs, format!("{REFS}/T/S/1/after\n{REFS}/T/S/1/before"));
        assert!(git(d.path(), &["branch", "--list"], &[], far()).unwrap().trim_start_matches("* ").starts_with("m"), "no branch made");

        let c = changes(d.path(), &before, &after, &[d.path().join("a.rs"), d.path().join("elsewhere.rs")]).unwrap();
        assert_eq!((c.before.as_str(), c.after.as_str()), (before.commit.as_str(), after.commit.as_str()));
        let by_path: HashMap<&str, &FileChange> = c.files.iter().map(|f| (f.path.as_str(), f)).collect();
        let a = by_path["a.rs"];
        assert_eq!((a.status, a.added, a.removed, a.authored), (ChangeStatus::Modified, Some(2), Some(1), true));
        assert_eq!((by_path["gone.txt"].status, by_path["gone.txt"].authored), (ChangeStatus::Deleted, false));
        assert_eq!((by_path["b.rs"].status, by_path["b.rs"].added), (ChangeStatus::Added, Some(1)));
        assert_eq!((by_path["pic.png"].added, by_path["pic.png"].removed), (None, None), "binary: no line counts");
        assert_eq!(c.totals(), (3, 2), "a.rs +2 −1, gone.txt −1, b.rs +1");
        assert!(!c.truncated);

        // Nothing changed: the same snapshot again, no files, no diff work.
        let again = capture(d.path(), &ref_name("T", "S", 2, Which::Before), BUDGET, Some(&after)).unwrap().unwrap();
        assert_eq!(again, after);
        assert!(changes(d.path(), &after, &again, &[]).unwrap().files.is_empty());
        assert_eq!(git(d.path(), &["rev-parse", &ref_name("T", "S", 2, Which::Before)], &[], far()).unwrap(), after.commit);
    }

    #[test]
    fn renames_keep_both_paths() {
        let d = repo();
        std::fs::write(d.path().join("old.rs"), "fn main() {}\nfn other() {}\nfn more() {}\n").unwrap();
        git(d.path(), &["add", "."], &[], far()).unwrap();
        git(d.path(), &["commit", "--quiet", "-m", "base"], &[], far()).unwrap();
        let before = take(d.path(), far()).unwrap().unwrap();
        std::fs::rename(d.path().join("old.rs"), d.path().join("new.rs")).unwrap();
        let after = take(d.path(), far()).unwrap().unwrap();
        let c = changes(d.path(), &before, &after, &[]).unwrap();
        assert_eq!(c.files.len(), 1);
        assert_eq!((c.files[0].path.as_str(), c.files[0].old_path.as_deref(), c.files[0].status), ("new.rs", Some("old.rs"), ChangeStatus::Renamed));
    }

    #[test]
    fn nothing_to_capture_outside_a_repository_or_before_its_first_commit() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(capture(d.path(), "refs/axel/checkpoints/x", BUDGET, None).unwrap(), None);
        git(d.path(), &["init", "--quiet"], &[], far()).unwrap();
        assert_eq!(capture(d.path(), "refs/axel/checkpoints/x", BUDGET, None).unwrap(), None, "no HEAD yet");
    }

    #[test]
    fn parses_numstat_and_name_status_together() {
        let numstat = "3\t1\tsrc/a.rs\0-\t-\timg.png\x000\t0\t\0old/x.rs\0new/x.rs\0";
        let status = "M\0src/a.rs\0A\0img.png\0R100\0old/x.rs\0new/x.rs\0";
        let files = parse(numstat, status);
        assert_eq!(files.len(), 3);
        assert_eq!((files[0].path.as_str(), files[0].status, files[0].added, files[0].removed), ("src/a.rs", ChangeStatus::Modified, Some(3), Some(1)));
        assert_eq!((files[1].path.as_str(), files[1].status, files[1].added), ("img.png", ChangeStatus::Added, None));
        assert_eq!((files[2].path.as_str(), files[2].old_path.as_deref(), files[2].status), ("new/x.rs", Some("old/x.rs"), ChangeStatus::Renamed));
        assert!(parse("", "").is_empty());
    }

    #[test]
    fn a_slow_git_is_killed_at_the_deadline() {
        let d = repo();
        let err = git(d.path(), &["-c", "alias.zz=!sleep 5", "zz"], &[], Instant::now() + Duration::from_millis(50)).unwrap_err();
        assert!(err.to_string().contains("time budget"), "{err}");
    }
}
