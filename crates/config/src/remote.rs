//! Remote skills: GitHub repositories (skills.sh links resolve to them), fetched with
//! `git`, pinned to a commit in `AXEL.lock` next to `AXEL.md`, and cached by commit.
//!
//! A pinned skill never changes under you: launches use the commit in the lock, and
//! fetch it only when it isn't cached yet. A source the lock doesn't know yet is fetched
//! at its ref (the default branch when none) and pinned on the spot. Moving a pin is an
//! explicit step ([`check_updates`], then [`Lock::save`]), so an upstream change to a
//! skill (instructions an agent follows with your permissions) is always reviewed.
//!
//! Cache layout: `<cache>/github/<owner>/<repo>/<commit>/`, a checkout without `.git`.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{Error, GitHubSkills, SKILL_FILE, Skill, SkillSource, WorkspaceConfig};

/// File name of the lock, next to `AXEL.md`.
pub const AXEL_LOCK: &str = "AXEL.lock";

const LOCK_HEADER: &str = "# Remote skills pinned by axel, one commit per source. Commit this file.\n# `axel skills update` moves the pins after showing what changed.\n";

/// `AXEL.lock`: the commit each remote source is pinned to, by [`GitHubSkills::key`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    #[serde(default)]
    pub skills: BTreeMap<String, String>,
}

impl Lock {
    /// Read `dir/AXEL.lock`; empty when it doesn't exist.
    pub fn load(dir: &Path) -> Result<Lock, Error> {
        let path = dir.join(AXEL_LOCK);
        match fs::read_to_string(&path) {
            Ok(text) if text.trim().is_empty() => Ok(Lock::default()),
            Ok(text) => serde_norway::from_str(&text).map_err(|e| Error::from(e).at(&path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Lock::default()),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    /// Write `dir/AXEL.lock`.
    pub fn save(&self, dir: &Path) -> Result<(), Error> {
        let path = dir.join(AXEL_LOCK);
        let body = serde_norway::to_string(self)?;
        fs::write(&path, format!("{LOCK_HEADER}{body}")).map_err(|e| Error::io(path, e))
    }
}

/// Where a source's skills are on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillRoot {
    /// A directory of skills (`<name>.md`, `<name>/SKILL.md`).
    Dir(PathBuf),
    /// Skill files picked out of a remote checkout.
    Files(Vec<PathBuf>),
}

/// Fetches GitHub repositories into the cache.
#[derive(Clone, Debug)]
pub struct Fetcher {
    cache: PathBuf,
    /// `https://github.com`, or `AXEL_GITHUB_URL` (tests point it at local repos).
    base_url: String,
}

impl Fetcher {
    pub fn new(cache: impl Into<PathBuf>) -> Fetcher {
        let base_url = std::env::var("AXEL_GITHUB_URL").ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| "https://github.com".into());
        Fetcher { cache: cache.into(), base_url: base_url.trim_end_matches('/').to_string() }
    }

    fn repo_dir(&self, g: &GitHubSkills) -> PathBuf {
        self.cache.join("github").join(&g.owner).join(&g.repo)
    }

    /// The checkout of `g` at `commit`, fetching it when it isn't cached.
    pub fn checkout(&self, g: &GitHubSkills, commit: &str) -> Result<PathBuf, Error> {
        let dir = self.repo_dir(g).join(commit);
        if dir.is_dir() {
            return Ok(dir);
        }
        let (got, dir) = self.fetch(g, commit)?;
        if got != commit {
            return Err(Error::Git(format!("{g}: asked for {commit}, got {got}")));
        }
        Ok(dir)
    }

    /// Fetch the tip of `g`'s ref (the default branch when none): its commit and checkout.
    pub fn latest(&self, g: &GitHubSkills) -> Result<(String, PathBuf), Error> {
        self.fetch(g, g.git_ref.as_deref().unwrap_or("HEAD"))
    }

    fn fetch(&self, g: &GitHubSkills, what: &str) -> Result<(String, PathBuf), Error> {
        let parent = self.repo_dir(g);
        fs::create_dir_all(&parent).map_err(|e| Error::io(&parent, e))?;
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let tmp = parent.join(format!(".fetch-{}-{nanos}", std::process::id()));
        let result = (|| {
            let url = format!("{}/{}/{}.git", self.base_url, g.owner, g.repo);
            git(None, &["init", "-q", tmp.to_str().unwrap_or_default()])?;
            git(Some(&tmp), &["fetch", "-q", "--depth", "1", &url, what]).map_err(|e| match e {
                Error::Git(msg) => Error::Git(format!("can't fetch {url}: {msg}")),
                other => other,
            })?;
            git(Some(&tmp), &["checkout", "-q", "FETCH_HEAD"])?;
            let commit = git(Some(&tmp), &["rev-parse", "HEAD"])?;
            fs::remove_dir_all(tmp.join(".git")).map_err(|e| Error::io(&tmp, e))?;
            let dest = parent.join(&commit);
            if dest.is_dir() {
                let _ = fs::remove_dir_all(&tmp);
            } else {
                fs::rename(&tmp, &dest).map_err(|e| Error::io(&dest, e))?;
            }
            Ok((commit, dest))
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&tmp);
        }
        result
    }
}

/// Run git (never prompting for credentials); its trimmed stdout.
fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, Error> {
    let mut cmd = Command::new("git");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    let out = cmd
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| Error::Git(format!("running git: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // git's first `fatal:` line says what went wrong; the rest are hints.
        let err = err
            .lines()
            .find_map(|l| l.trim().strip_prefix("fatal: "))
            .or_else(|| err.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or("failed")
            .trim();
        return Err(Error::Git(err.to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The `SKILL.md` files in a checkout: the one named `skill` (by directory or
/// frontmatter name), or all of them. Hidden directories (`.github`, `.agents`, ...)
/// are a repo's own tooling, so they count only when nothing else matches.
pub fn skill_files(checkout: &Path, skill: Option<&str>) -> Vec<PathBuf> {
    let mut all = Vec::new();
    walk(checkout, &mut all);
    let hidden = |p: &Path| {
        p.strip_prefix(checkout).unwrap_or(p).components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    };
    let pick = |files: Vec<PathBuf>| -> Vec<PathBuf> {
        let (shown, hidden): (Vec<_>, Vec<_>) = files.into_iter().partition(|p| !hidden(p));
        if shown.is_empty() { hidden } else { shown }
    };
    let mut files = match skill {
        None => pick(all),
        Some(name) => {
            let matches: Vec<PathBuf> = all
                .into_iter()
                .filter(|p| {
                    p.parent().and_then(Path::file_name).is_some_and(|d| d == name)
                        || Skill::from_file(p).is_ok_and(|s| s.name == name)
                })
                .collect();
            let mut m = pick(matches);
            m.sort_by_key(|p| p.components().count());
            m.truncate(1);
            m
        }
    };
    files.sort();
    files
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if path.is_dir() {
            if name != ".git" && name != "node_modules" {
                walk(&path, out);
            }
        } else if name == SKILL_FILE {
            out.push(path);
        }
    }
}

/// A manifest's sources, resolved to where their skills are on disk.
#[derive(Clone, Debug, Default)]
pub struct Resolved {
    pub roots: Vec<SkillRoot>,
    /// Sources that couldn't be used, and why; launches go on without them.
    pub warnings: Vec<String>,
}

/// Resolve `config`'s skill sources. Remote ones use their pin in `AXEL.lock`; new
/// ones are fetched and pinned (the lock is written). Never moves an existing pin.
pub fn resolve(config: &WorkspaceConfig, cache: &Path) -> Resolved {
    let mut out = Resolved::default();
    let dir = config.dir().map(Path::to_path_buf);
    let mut lock = match &dir {
        Some(d) => Lock::load(d).unwrap_or_else(|e| {
            out.warnings.push(format!("{e}; ignoring it"));
            Lock::default()
        }),
        None => Lock::default(),
    };
    let fetcher = Fetcher::new(cache);
    let mut pinned_new = false;
    for source in &config.skills {
        match source {
            SkillSource::Dir(_) => {
                if let Some(p) = source.resolve(config.dir()) {
                    out.roots.push(SkillRoot::Dir(p));
                }
            }
            SkillSource::GitHub(g) => {
                let key = g.key();
                let checkout = match lock.skills.get(&key) {
                    Some(commit) => fetcher.checkout(g, commit),
                    None => fetcher.latest(g).map(|(commit, dir)| {
                        lock.skills.insert(key.clone(), commit);
                        pinned_new = true;
                        dir
                    }),
                };
                match checkout {
                    Ok(checkout) => {
                        let files = skill_files(&checkout, g.skill.as_deref());
                        if files.is_empty() {
                            out.warnings.push(match &g.skill {
                                Some(s) => format!("{g}: no skill `{s}` in the repo"),
                                None => format!("{g}: no SKILL.md in the repo"),
                            });
                        } else {
                            out.roots.push(SkillRoot::Files(files));
                        }
                    }
                    Err(e) => out.warnings.push(format!("skipping {g}: {e}")),
                }
            }
        }
    }
    if pinned_new && let Some(d) = &dir {
        // Forget pins for sources the manifest no longer lists.
        let keys: Vec<String> = config.remote_skills().iter().map(|g| g.key()).collect();
        lock.skills.retain(|k, _| keys.contains(k));
        if let Err(e) = lock.save(d) {
            out.warnings.push(format!("couldn't write {AXEL_LOCK}: {e}"));
        }
    }
    out
}

/// A remote source whose ref has moved past its pin.
#[derive(Clone, Debug)]
pub struct Update {
    pub source: GitHubSkills,
    /// The pin now; `None` when the source isn't pinned yet.
    pub from: Option<String>,
    pub to: String,
    /// The skill directories at the pin, and at the new commit.
    pub old: Vec<PathBuf>,
    pub new: Vec<PathBuf>,
}

/// Fetch every remote source's ref and report those that moved. Nothing is changed:
/// apply an update by setting `lock.skills[source.key()] = to` and saving.
pub fn check_updates(config: &WorkspaceConfig, cache: &Path) -> (Lock, Vec<Update>, Vec<String>) {
    let lock = config.dir().map(|d| Lock::load(d).unwrap_or_default()).unwrap_or_default();
    let fetcher = Fetcher::new(cache);
    let mut updates = Vec::new();
    let mut errors = Vec::new();
    let skill_dirs = |dir: &Path, g: &GitHubSkills| -> Vec<PathBuf> {
        skill_files(dir, g.skill.as_deref()).iter().filter_map(|f| f.parent().map(Path::to_path_buf)).collect()
    };
    for g in config.remote_skills() {
        let from = lock.skills.get(&g.key()).cloned();
        match fetcher.latest(g) {
            Ok((to, dir)) if from.as_deref() != Some(to.as_str()) => {
                let old = from.as_deref().and_then(|c| fetcher.checkout(g, c).ok()).map(|d| skill_dirs(&d, g)).unwrap_or_default();
                updates.push(Update { source: g.clone(), from, to, old, new: skill_dirs(&dir, g) });
            }
            Ok(_) => {}
            Err(e) => errors.push(format!("{g}: {e}")),
        }
    }
    (lock, updates, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare repo at `<root>/<owner>/<repo>.git` with `files` committed; its commit.
    fn repo(root: &Path, owner: &str, repo: &str, files: &[(&str, &str)]) -> String {
        let work = root.join("work").join(owner).join(repo);
        fs::create_dir_all(&work).unwrap();
        for (path, text) in files {
            let p = work.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
        let g = |args: &[&str]| git(Some(&work), args).unwrap();
        if !work.join(".git").exists() {
            g(&["init", "-q", "-b", "main"]);
        }
        g(&["add", "-A"]);
        g(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "c", "--allow-empty"]);
        let bare = root.join("remote").join(owner).join(format!("{repo}.git"));
        if !bare.exists() {
            fs::create_dir_all(bare.parent().unwrap()).unwrap();
            git(None, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]).unwrap();
            git(Some(&bare), &["config", "uploadpack.allowAnySHA1InWant", "true"]).unwrap();
        } else {
            g(&["push", "-q", bare.to_str().unwrap(), "main"]);
        }
        g(&["rev-parse", "HEAD"])
    }

    fn manifest(dir: &Path, skills: &[&str]) -> WorkspaceConfig {
        let list: String = skills.iter().map(|s| format!("  - {s}\n")).collect();
        fs::write(dir.join("AXEL.md"), format!("---\nskills:\n{list}---\n# p\n")).unwrap();
        WorkspaceConfig::load(dir).unwrap().unwrap()
    }

    #[test]
    fn picks_skills_out_of_a_checkout() {
        let root = tempfile::tempdir().unwrap();
        let c = root.path();
        for p in ["skills/engineering/code-review/SKILL.md", "skills/grill-me/SKILL.md", ".agents/skills/internal/SKILL.md", "other/renamed/SKILL.md"] {
            fs::create_dir_all(c.join(p).parent().unwrap()).unwrap();
            fs::write(c.join(p), if p.starts_with("other") { "---\nname: by-name\n---\n# x\n" } else { "# x\n" }).unwrap();
        }
        let names = |v: Vec<PathBuf>| -> Vec<String> { v.iter().map(|p| p.strip_prefix(c).unwrap().display().to_string()).collect() };
        assert_eq!(names(skill_files(c, Some("code-review"))), ["skills/engineering/code-review/SKILL.md"]);
        assert_eq!(names(skill_files(c, Some("by-name"))), ["other/renamed/SKILL.md"]);
        assert_eq!(names(skill_files(c, Some("internal"))), [".agents/skills/internal/SKILL.md"], "hidden only as a fallback");
        assert!(skill_files(c, Some("nope")).is_empty());
        assert_eq!(names(skill_files(c, None)).len(), 3, "a whole repo leaves out hidden dirs");
    }

    #[test]
    fn pins_on_first_use_and_updates_only_when_asked() {
        let root = tempfile::tempdir().unwrap();
        let first = repo(root.path(), "acme", "skills", &[("skills/review/SKILL.md", "---\nname: review\n---\nv1\n"), ("skills/review/scripts/run.sh", "echo\n")]);
        // SAFETY: tests in this module are the only readers of this variable.
        unsafe { std::env::set_var("AXEL_GITHUB_URL", format!("file://{}", root.path().join("remote").display())) };
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let cache = root.path().join("cache");
        let config = manifest(&project, &["acme/skills/review", "https://skills.sh/acme/skills/missing"]);

        let r = resolve(&config, &cache);
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("no skill `missing`"));
        let SkillRoot::Files(files) = &r.roots[0] else { panic!("{:?}", r.roots) };
        assert!(files[0].ends_with(format!("{first}/skills/review/SKILL.md")));
        assert!(files[0].parent().unwrap().join("scripts/run.sh").is_file(), "the whole skill dir is there");
        let lock = Lock::load(&project).unwrap();
        assert_eq!(lock.skills.get("acme/skills/review"), Some(&first));
        assert!(fs::read_to_string(project.join(AXEL_LOCK)).unwrap().starts_with("# Remote skills"));

        // Upstream moves: launches keep the pin; check_updates reports it.
        let second = repo(root.path(), "acme", "skills", &[("skills/review/SKILL.md", "---\nname: review\n---\nv2\n")]);
        let r = resolve(&config, &cache);
        let SkillRoot::Files(files) = &r.roots[0] else { panic!() };
        assert!(files[0].to_string_lossy().contains(&first));
        let (mut lock, updates, errors) = check_updates(&config, &cache);
        assert!(errors.is_empty(), "{errors:?}");
        let review: Vec<&Update> = updates.iter().filter(|u| u.source.skill.as_deref() == Some("review")).collect();
        assert_eq!((review[0].from.as_deref(), review[0].to.as_str()), (Some(first.as_str()), second.as_str()));
        assert!(fs::read_to_string(review[0].new[0].join("SKILL.md")).unwrap().contains("v2"));
        assert!(fs::read_to_string(review[0].old[0].join("SKILL.md")).unwrap().contains("v1"));
        lock.skills.insert(review[0].source.key(), second.clone());
        lock.save(&project).unwrap();

        // A fresh cache fetches the pinned commit, not the tip.
        let r = resolve(&config, &root.path().join("cache2"));
        let SkillRoot::Files(files) = &r.roots[0] else { panic!() };
        assert!(files[0].to_string_lossy().contains(&second));

        // A repo that doesn't exist is a warning, not a failure.
        let config = manifest(&project, &["github:acme/nope"]);
        let r = resolve(&config, &cache);
        assert!(r.roots.is_empty() && r.warnings[0].starts_with("skipping acme/nope: git: can't fetch"), "{:?}", r.warnings);
    }
}
