//! `axel gc`: prune old turn checkpoints.  Checkpoint refs keep objects alive,
//! so they go once a thread is archived (after a week) or a turn is old (a
//! month, keeping the last turns of each session).  Git's own gc then
//! reclaims the objects.  Runs on demand, and after `axel thread archive`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use axel_acp::checkpoint::REFS;
use axel_store::{ThreadFilter, ThreadState};

use super::Ctx;

/// Checkpoints of an archived or deleted thread live this long.
pub const ARCHIVED_KEEP_SECS: i64 = 7 * 24 * 3600;
/// Checkpoints of a live thread are all kept this long…
pub const LIVE_KEEP_SECS: i64 = 30 * 24 * 3600;
/// …and after that only the latest turns of each session.
pub const KEEP_TURNS: usize = 20;

/// What the pruner knows about a thread.
#[derive(Clone, Debug, Default)]
pub struct ThreadInfo {
    /// `None`: a thread this store doesn't know (another replica's): kept.
    pub state: Option<ThreadState>,
    /// When each turn happened (Unix seconds), by session and ordinal.
    pub turn_at: HashMap<(String, i64), i64>,
}

/// One checkpoint ref, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckpointRef {
    name: String,
    thread: String,
    /// `Some((session, ordinal))` for a turn's checkpoint, `None` for an undo snapshot.
    turn: Option<(String, i64)>,
    /// The commit's date (Unix seconds), the fallback age.
    created: i64,
}

fn parse_ref(line: &str) -> Option<CheckpointRef> {
    let (name, date) = line.split_once('\t')?;
    let rest = name.strip_prefix(REFS)?.strip_prefix('/')?;
    let parts: Vec<&str> = rest.split('/').collect();
    let turn = match parts.as_slice() {
        [_, "undo", _] => None,
        [_, session, ordinal, _] => Some(((*session).to_string(), ordinal.parse().ok()?)),
        _ => return None,
    };
    Some(CheckpointRef { name: name.to_string(), thread: parts[0].to_string(), turn, created: date.trim().parse().ok()? })
}

/// Which of `refs` to delete at `now`, given each thread's `info`.
fn expired(refs: &[CheckpointRef], now: i64, info: &dyn Fn(&str) -> ThreadInfo) -> Vec<String> {
    let mut by_thread: HashMap<&str, Vec<&CheckpointRef>> = HashMap::new();
    for r in refs {
        by_thread.entry(r.thread.as_str()).or_default().push(r);
    }
    let mut out = Vec::new();
    for (thread, refs) in by_thread {
        let info = info(thread);
        let Some(state) = info.state else { continue };
        let archived = matches!(state, ThreadState::Archived | ThreadState::Deleted);
        let keep_secs = if archived { ARCHIVED_KEEP_SECS } else { LIVE_KEEP_SECS };
        // The latest turns of each session stay on a live thread.
        let mut ordinals: HashMap<&str, Vec<i64>> = HashMap::new();
        for r in &refs {
            if let Some((s, o)) = &r.turn {
                ordinals.entry(s.as_str()).or_default().push(*o);
            }
        }
        let mut recent: HashSet<(String, i64)> = HashSet::new();
        for (s, mut os) in ordinals {
            os.sort_unstable();
            os.dedup();
            for o in os.iter().rev().take(KEEP_TURNS) {
                recent.insert((s.to_string(), *o));
            }
        }
        for r in refs {
            let at = r.turn.as_ref().and_then(|k| info.turn_at.get(k).copied()).unwrap_or(r.created);
            let old = now - at > keep_secs;
            let protected = !archived && r.turn.as_ref().is_some_and(|k| recent.contains(k));
            if old && !protected {
                out.push(r.name.clone());
            }
        }
    }
    out.sort();
    out
}

/// Delete the expired checkpoint refs of the repository at `dir`; the names deleted.
pub fn prune(dir: &Path, now: i64, info: &dyn Fn(&str) -> ThreadInfo) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["for-each-ref", "--format=%(refname)%09%(creatordate:unix)", REFS])
        .output()
        .context("running git for-each-ref")?;
    if !out.status.success() {
        bail!("git for-each-ref: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let refs: Vec<CheckpointRef> = String::from_utf8_lossy(&out.stdout).lines().filter_map(parse_ref).collect();
    let gone = expired(&refs, now, info);
    for name in &gone {
        let del = Command::new("git").arg("-C").arg(dir).args(["update-ref", "-d", name]).output()?;
        if !del.status.success() {
            bail!("git update-ref -d {name}: {}", String::from_utf8_lossy(&del.stderr).trim());
        }
    }
    Ok(gone)
}

/// Every repository this store's projects and thread worktrees live in.
fn repositories(ctx: &Ctx) -> Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = ctx.store.recent_projects(10_000)?.into_iter().map(|p| p.path).collect();
    for t in ctx.store.list_threads(ThreadFilter::default())? {
        for tp in ctx.store.thread_projects(&t.id)? {
            if let Some(w) = tp.workspace.worktree_path {
                dirs.push(w);
            }
        }
    }
    // One entry per repository: worktrees share their common dir.
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for d in dirs {
        if !d.is_dir() {
            continue;
        }
        let common = Command::new("git").arg("-C").arg(&d).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).output();
        let Ok(c) = common else { continue };
        if !c.status.success() {
            continue;
        }
        let key = String::from_utf8_lossy(&c.stdout).trim().to_string();
        if seen.insert(key) {
            out.push(d);
        }
    }
    Ok(out)
}

/// Prune every repository; `quiet` says nothing unless something went.
pub fn run(ctx: &Ctx, quiet: bool) -> Result<usize> {
    let now = chrono::Utc::now().timestamp();
    let cache: std::cell::RefCell<HashMap<String, ThreadInfo>> = std::cell::RefCell::new(HashMap::new());
    let store = &ctx.store;
    let info = |thread: &str| -> ThreadInfo {
        if let Some(i) = cache.borrow().get(thread) {
            return i.clone();
        }
        let state = store.get_thread(thread).ok().flatten().map(|t| t.state);
        let turn_at = store
            .turns(thread)
            .unwrap_or_default()
            .into_iter()
            .map(|t| ((t.session_id, t.summary.ordinal), t.created_at as i64))
            .collect();
        let i = ThreadInfo { state, turn_at };
        cache.borrow_mut().insert(thread.to_string(), i.clone());
        i
    };
    let mut total = 0;
    for dir in repositories(ctx)? {
        match prune(&dir, now, &info) {
            Ok(gone) => {
                total += gone.len();
                if !quiet && !gone.is_empty() {
                    println!("{}: {} checkpoint ref{} pruned", dir.display(), gone.len(), if gone.len() == 1 { "" } else { "s" });
                }
            }
            Err(e) => {
                if !quiet {
                    eprintln!("{}: {e:#}", dir.display());
                }
            }
        }
    }
    if !quiet {
        println!("{total} checkpoint ref{} pruned", if total == 1 { "" } else { "s" });
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(name: &str, created: i64) -> CheckpointRef {
        parse_ref(&format!("{name}\t{created}")).unwrap()
    }

    #[test]
    fn refs_parse_and_expire_by_thread_state_and_age() {
        let day = 24 * 3600;
        let now = 1_000 * day;
        let p = |rest: &str, age_days: i64| r(&format!("{REFS}/{rest}"), now - age_days * day);
        assert!(parse_ref("refs/heads/main\t1").is_none());
        assert!(parse_ref(&format!("{REFS}/T/S/x/before\t1")).is_none());
        let undo = p("T1/undo/3", 40);
        assert_eq!(undo.turn, None);
        let refs = vec![
            p("T1/S1/1/before", 40),
            p("T1/S1/1/after", 40),
            p("T1/S1/2/before", 1),
            p("T2/S2/1/before", 10),
            p("T2/S2/2/before", 10),
            p("T3/S3/1/before", 400),
            undo,
        ];
        let info = |t: &str| match t {
            "T1" => ThreadInfo { state: Some(ThreadState::Open), ..Default::default() },
            "T2" => ThreadInfo { state: Some(ThreadState::Archived), turn_at: HashMap::from([(("S2".to_string(), 2), now - 2 * day)]) },
            _ => ThreadInfo::default(),
        };
        let gone = expired(&refs, now, &info);
        // T1 is live: turn 1 is 40 days old but among the last 20 of its session: kept; the undo snapshot goes.
        // T2 is archived: turn 1 is 10 days old by its commit (gone); turn 2 is 2 days old by the store (kept).
        // T3 is unknown here: kept.
        assert_eq!(gone, vec![format!("{REFS}/T1/undo/3"), format!("{REFS}/T2/S2/1/before")]);
        // A live session with 25 old turns keeps its latest 20.
        let many: Vec<CheckpointRef> = (1..=25).map(|o| p(&format!("T1/S1/{o}/before"), 60)).collect();
        let gone = expired(&many, now, &info);
        assert_eq!(gone.len(), 5);
        assert!(gone.contains(&format!("{REFS}/T1/S1/5/before")) && !gone.contains(&format!("{REFS}/T1/S1/6/before")));
    }

    #[test]
    fn prune_deletes_refs_in_a_repository() {
        let d = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(d.path()).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "--quiet"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false", "commit", "--quiet", "--allow-empty", "-m", "root"]);
        let head = git(&["rev-parse", "HEAD"]);
        git(&["update-ref", &format!("{REFS}/T/S/1/before"), &head]);
        git(&["update-ref", &format!("{REFS}/T/S/1/after"), &head]);
        let far = chrono::Utc::now().timestamp() + 400 * 24 * 3600;
        let archived = |_: &str| ThreadInfo { state: Some(ThreadState::Archived), ..Default::default() };
        let gone = prune(d.path(), far, &archived).unwrap();
        assert_eq!(gone.len(), 2);
        assert!(git(&["for-each-ref", REFS]).is_empty());
    }
}
