//! Thin git helpers (shelling out to `git`): repo root, current branch, worktrees.
//! Ported from Axel v1 `git.rs` without the isolated-worktree squash/cherry-pick flow.

use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{Error, config::AXEL_MD};

fn run_git<I, S>(dir: &Path, args: I) -> Result<String, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let output = Command::new("git")
        .args(&args)
        .current_dir(dir)
        .output()
        .map_err(|e| Error::Git(format!("failed to run git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let shown: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        return Err(Error::Git(if stderr.is_empty() {
            format!("`git {}` failed", shown.join(" "))
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether a `git` executable is on `PATH`.
pub fn is_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Top-level directory of the repository (or worktree) containing `dir`.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    run_git(dir, ["rev-parse", "--show-toplevel"])
        .ok()
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
}

/// Current branch name. `None` outside a repository or when `HEAD` is detached.
pub fn current_branch(dir: &Path) -> Option<String> {
    if !dir.is_dir() {
        return None;
    }
    run_git(dir, ["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .filter(|branch| !branch.is_empty() && branch != "HEAD")
}

/// Commit hash of `HEAD`.
pub fn head(dir: &Path) -> Option<String> {
    if !dir.is_dir() {
        return None;
    }
    run_git(dir, ["rev-parse", "HEAD"])
        .ok()
        .filter(|h| !h.is_empty())
}

/// Whether `branch` exists locally.
pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    run_git(
        repo,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// `remote/branch` for the first remote that has `branch`.
fn remote_branch(repo: &Path, branch: &str) -> Option<String> {
    run_git(
        repo,
        [
            "for-each-ref",
            "--format=%(refname:short)",
            &format!("refs/remotes/*/{branch}"),
        ],
    )
    .ok()
    .and_then(|out| out.lines().next().map(str::trim).map(str::to_string))
    .filter(|r| !r.is_empty())
}

/// All worktrees of the repository as `(path, branch)`. A detached worktree reports
/// `"HEAD"`; a bare entry reports `""`.
pub fn list_worktrees(repo: &Path) -> Result<Vec<(PathBuf, String)>, Error> {
    let out = run_git(repo, ["worktree", "list", "--porcelain"])?;
    let mut worktrees = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;

    let mut flush = |path: &mut Option<PathBuf>, branch: &mut Option<String>| {
        if let Some(p) = path.take() {
            worktrees.push((p, branch.take().unwrap_or_default()));
        }
    };

    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch);
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if line == "detached" {
            branch = Some("HEAD".to_string());
        }
    }
    flush(&mut path, &mut branch);
    Ok(worktrees)
}

/// Turn a thread title into a branch name: `Fix the Login Bug!` -> `fix-the-login-bug`.
pub fn suggest_branch(title: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if c == '/' || c == '.' {
            if !last_dash {
                out.push(c);
                last_dash = false;
            }
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '/' || c == '.');
    if trimmed.is_empty() {
        "work".to_string()
    } else {
        trimmed
            .chars()
            .take(60)
            .collect::<String>()
            .trim_end_matches('-')
            .to_string()
    }
}

/// Conventional worktree location: a sibling of the repo named `<repo>-<branch>` with
/// path separators in the branch replaced by `-`.
pub fn default_worktree_path(repo_root: &Path, branch: &str) -> PathBuf {
    let name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let parent = repo_root.parent().unwrap_or(repo_root);
    parent.join(format!("{name}-{}", branch.replace(['/', '\\'], "-")))
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Make sure a worktree for `branch` exists and return its path.
///
/// - If a worktree is already checked out on `branch`, its path is returned (even if it
///   is not `dest`).
/// - Otherwise a worktree is added at `dest`: on the existing local branch, tracking a
///   remote branch of that name, or on a new branch forked from the repo's `HEAD`.
/// - `AXEL.md` from the repo root is symlinked into the worktree when the worktree has
///   none (e.g. it is untracked).
///
/// A relative `dest` is resolved against the current directory.
pub fn ensure_worktree(repo: &Path, branch: &str, dest: &Path) -> Result<PathBuf, Error> {
    let branch = branch.trim();
    if branch.is_empty() {
        return Err(Error::Invalid("branch name is empty".into()));
    }
    let root = repo_root(repo)
        .ok_or_else(|| Error::Git(format!("{} is not inside a git repository", repo.display())))?;
    let dest = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| Error::io(dest, e))?
            .join(dest)
    };

    let mut worktrees = list_worktrees(&root)?;
    if let Some((path, _)) = worktrees.iter().find(|(_, b)| b == branch) {
        if path.exists() {
            return Ok(path.clone());
        }
        run_git(&root, ["worktree", "prune"])?;
        worktrees = list_worktrees(&root)?;
    }

    if let Some((path, b)) = worktrees.iter().find(|(p, _)| same_path(p, &dest)) {
        return Err(Error::Git(format!(
            "{} is already a worktree on branch `{b}`",
            path.display()
        )));
    }
    if dest.exists() {
        let empty = fs::read_dir(&dest)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !empty {
            return Err(Error::Git(format!(
                "{} already exists and is not empty",
                dest.display()
            )));
        }
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    let dest_arg = dest.as_os_str();
    if branch_exists(&root, branch) {
        run_git(
            &root,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                dest_arg,
                OsStr::new(branch),
            ],
        )?;
    } else if let Some(remote) = remote_branch(&root, branch) {
        run_git(
            &root,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--track"),
                OsStr::new("-b"),
                OsStr::new(branch),
                dest_arg,
                OsStr::new(&remote),
            ],
        )?;
    } else {
        run_git(
            &root,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-b"),
                OsStr::new(branch),
                dest_arg,
                OsStr::new("HEAD"),
            ],
        )?;
    }

    let main_manifest = root.join(AXEL_MD);
    let worktree_manifest = dest.join(AXEL_MD);
    if main_manifest.is_file()
        && !worktree_manifest.exists()
        && !crate::drivers::is_symlink(&worktree_manifest)
    {
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&main_manifest, &worktree_manifest);
    }

    Ok(dest)
}

/// Remove a worktree. With `force`, discard uncommitted changes in it.
pub fn remove_worktree(repo: &Path, worktree: &Path, force: bool) -> Result<(), Error> {
    let mut args = vec![OsString::from("worktree"), OsString::from("remove")];
    if force {
        args.push(OsString::from("--force"));
    }
    args.push(worktree.as_os_str().to_os_string());
    run_git(repo, args)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp repo with one commit on `main`, or `None` when git is unavailable.
    fn temp_repo() -> Option<(tempfile::TempDir, PathBuf)> {
        if !is_available() {
            eprintln!("git not available; skipping");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args([
                    "-c",
                    "init.defaultBranch=main",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                ])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        };
        git(&["init", "-q"]);
        fs::write(repo.join("README.md"), "hi\n").unwrap();
        fs::write(repo.join(AXEL_MD), "---\nworkspace: t\n---\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "init"]);
        Some((dir, repo))
    }

    #[test]
    fn root_and_branch() {
        let Some((dir, repo)) = temp_repo() else {
            return;
        };
        let root = repo_root(&repo).unwrap();
        assert!(same_path(&root, &repo));
        assert_eq!(current_branch(&repo).as_deref(), Some("main"));
        assert!(head(&repo).is_some_and(|h| h.len() == 40));
        assert!(branch_exists(&repo, "main"));
        assert!(!branch_exists(&repo, "nope"));

        let not_repo = dir.path().join("plain");
        fs::create_dir(&not_repo).unwrap();
        // The temp dir itself may live inside another repo on a dev machine, so only
        // assert on a path that does not exist.
        assert!(repo_root(&dir.path().join("missing")).is_none());
        assert!(current_branch(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn worktrees_round_trip() {
        let Some((dir, repo)) = temp_repo() else {
            return;
        };
        let dest = dir.path().join("repo-feat-x");

        let path = ensure_worktree(&repo, "feat/x", &dest).unwrap();
        assert!(same_path(&path, &dest));
        assert_eq!(current_branch(&dest).as_deref(), Some("feat/x"));
        assert!(dest.join("README.md").is_file());
        // AXEL.md was untracked in the main repo, so it is linked in.
        assert!(crate::drivers::is_symlink(&dest.join(AXEL_MD)));

        let list = list_worktrees(&repo).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|(p, b)| b == "main" && same_path(p, &repo)));
        assert!(
            list.iter()
                .any(|(p, b)| b == "feat/x" && same_path(p, &dest))
        );

        // Idempotent: same branch again returns the existing worktree even with another dest.
        let again = ensure_worktree(&repo, "feat/x", &dir.path().join("elsewhere")).unwrap();
        assert!(same_path(&again, &dest));

        // Existing local branch, new worktree.
        Command::new("git")
            .args(["branch", "feat/y"])
            .current_dir(&repo)
            .output()
            .unwrap();
        let dest_y = dir.path().join("repo-feat-y");
        ensure_worktree(&repo, "feat/y", &dest_y).unwrap();
        assert_eq!(current_branch(&dest_y).as_deref(), Some("feat/y"));

        // A dest that is already a worktree on another branch is refused.
        let err = ensure_worktree(&repo, "feat/z", &dest).unwrap_err();
        assert!(matches!(err, Error::Git(_)), "{err}");
        assert!(ensure_worktree(&repo, "  ", &dest).is_err());

        remove_worktree(&repo, &dest_y, true).unwrap();
        assert_eq!(list_worktrees(&repo).unwrap().len(), 2);
    }

    #[test]
    fn branch_suggestions() {
        assert_eq!(suggest_branch("Fix the Login Bug!"), "fix-the-login-bug");
        assert_eq!(suggest_branch("  Add   OAuth2 / PKCE  "), "add-oauth2-pkce");
        assert_eq!(suggest_branch("feat/auth.v2"), "feat/auth.v2");
        assert_eq!(suggest_branch("???"), "work");
        assert_eq!(
            default_worktree_path(Path::new("/code/app"), "feat/x"),
            PathBuf::from("/code/app-feat-x")
        );
    }
}
