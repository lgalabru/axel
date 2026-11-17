//! Skills a prompt names (`/name`), made the harness's own for the turn.
//!
//! The host finds the skills a prompt names among those installed (see
//! `axel_config::named_skills`) and sends them with the prompt as
//! `_meta.axel.skills: [{name, path}]`, to adapters that advertise
//! `_meta.axel.skills` in `initialize`. An adapter then links each skill its
//! harness doesn't load already into a directory of the session's own, outside
//! the project, and has the harness load it: Claude Code reads it through
//! `--add-dir` and `reload_skills`, Codex as an extra skill root.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};

/// A skill the prompt names: its name and its markdown file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requested {
    pub name: String,
    pub path: PathBuf,
}

/// `_meta` for a prompt naming `skills`.
pub fn prompt_meta(skills: &[&axel_config::SkillHint]) -> serde_json::Map<String, Value> {
    let list: Vec<Value> = skills.iter().map(|s| json!({ "name": s.name, "path": s.path })).collect();
    let mut meta = serde_json::Map::new();
    meta.insert("axel".into(), json!({ "skills": list }));
    meta
}

/// The skills a prompt request names (`_meta.axel.skills`). A name that
/// couldn't be a directory's (empty, hidden, or with a path separator) is
/// left out.
pub fn requested(params: &Value) -> Vec<Requested> {
    params
        .pointer("/_meta/axel/skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let name = s.get("name")?.as_str()?.to_string();
            let path = PathBuf::from(s.get("path")?.as_str()?);
            let plain = !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\']);
            (plain && path.is_absolute()).then_some(Requested { name, path })
        })
        .collect()
}

/// A session's own skill directory: links to the skills its prompts named
/// that the harness didn't load already. Removed with the session.
#[derive(Debug)]
pub struct SessionSkills {
    root: PathBuf,
    linked: Mutex<HashSet<String>>,
}

impl SessionSkills {
    /// A directory for session `id` under the system's temporary directory,
    /// this process's own (another opening the same session gets another),
    /// made when something is linked into it. Those left by processes that
    /// have gone (killed before they could remove theirs) go now.
    pub fn new(id: &str) -> Self {
        let temp = std::env::temp_dir();
        sweep(&temp);
        Self::at(temp.join(format!("{PREFIX}{id}-{}", std::process::id())))
    }

    pub fn at(root: PathBuf) -> Self {
        Self { root, linked: Mutex::new(HashSet::new()) }
    }

    /// The directory, made now: for a harness that must be pointed at it
    /// before anything is linked.
    pub fn made(&self) -> &Path {
        let _ = std::fs::create_dir_all(&self.root);
        &self.root
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Link each of `skills` not linked yet as `<root>/<under>/<name>`, a
    /// directory holding its `SKILL.md`: the skill's own directory, or for
    /// a single-file skill a directory made for it. The names newly linked;
    /// one that can't be linked is left out, and the caller can say so.
    pub fn link(&self, under: &Path, skills: &[Requested]) -> Vec<String> {
        let dir = self.root.join(under);
        let mut linked = self.linked.lock().expect("linked lock");
        let mut out = Vec::new();
        for s in skills {
            // A skill whose file is gone isn't linked: the harness couldn't load it.
            if linked.contains(&s.name) || !s.path.is_file() || std::fs::create_dir_all(&dir).is_err() {
                continue;
            }
            let at = dir.join(&s.name);
            let ok = if s.path.file_name().is_some_and(|f| f == axel_config::SKILL_FILE) {
                s.path.parent().is_some_and(|src| symlink(src, &at).is_ok())
            } else {
                std::fs::create_dir_all(&at).is_ok() && symlink(&s.path, &at.join(axel_config::SKILL_FILE)).is_ok()
            };
            if ok {
                linked.insert(s.name.clone());
                out.push(s.name.clone());
            }
        }
        out
    }

    /// Where a linked skill's `SKILL.md` is.
    pub fn skill_file(&self, under: &Path, name: &str) -> PathBuf {
        self.root.join(under).join(name).join(axel_config::SKILL_FILE)
    }
}

impl Drop for SessionSkills {
    fn drop(&mut self) {
        // Links only: removing them leaves the skills they point at alone.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What every session skill directory's name starts with; it ends `-<pid>`.
const PREFIX: &str = "axel-skills-";

/// Remove the session skill directories in `temp` whose process has gone.
fn sweep(temp: &Path) {
    for entry in std::fs::read_dir(temp).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix(PREFIX).and_then(|rest| rest.rsplit_once('-')).and_then(|(_, pid)| pid.parse::<u32>().ok()) else { continue };
        if pid != std::process::id() && !running(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(unix)]
fn running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return true };
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) }
}

#[cfg(not(unix))]
fn running(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
fn symlink(src: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, link)
}

#[cfg(not(unix))]
fn symlink(src: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_of_processes_that_have_gone_are_swept_and_live_ones_kept() {
        let temp = tempfile::tempdir().unwrap();
        // A process that has exited: its pid is free (for now).
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        let mine = std::process::id();
        for name in [format!("{PREFIX}s1-{gone}"), format!("{PREFIX}s2-{mine}"), format!("{PREFIX}no-pid"), "other-1".to_string()] {
            std::fs::create_dir_all(temp.path().join(name)).unwrap();
        }
        sweep(temp.path());
        let mut left: Vec<String> = std::fs::read_dir(temp.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["axel-skills-no-pid".to_string(), format!("{PREFIX}s2-{mine}"), "other-1".to_string()]);
    }

    #[test]
    fn named_skills_go_in_the_prompt_meta_and_come_out_checked() {
        let hint = |n: &str| axel_config::SkillHint { name: n.into(), description: None, scope: axel_config::SkillScope::User, path: format!("/s/{n}/SKILL.md").into() };
        let (rex, review) = (hint("rex"), hint("review"));
        let params = json!({ "_meta": prompt_meta(&[&rex, &review]) });
        assert_eq!(requested(&params), [Requested { name: "rex".into(), path: "/s/rex/SKILL.md".into() }, Requested { name: "review".into(), path: "/s/review/SKILL.md".into() }]);
        let odd = json!({ "_meta": { "axel": { "skills": [{ "name": "../x", "path": "/a/SKILL.md" }, { "name": ".hidden", "path": "/a/SKILL.md" }, { "name": "rel", "path": "a/SKILL.md" }] } } });
        assert!(requested(&odd).is_empty(), "no escaping the session's directory");
        assert!(requested(&json!({})).is_empty());
    }

    #[test]
    fn links_each_skill_once_as_a_directory_and_removes_only_the_links() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("rex")).unwrap();
        std::fs::write(src.path().join("rex/SKILL.md"), "rex").unwrap();
        std::fs::write(src.path().join("flat.md"), "flat").unwrap();
        let root = tempfile::tempdir().unwrap().path().join("session");
        let skills = SessionSkills::at(root.clone());
        let want = [
            Requested { name: "rex".into(), path: src.path().join("rex/SKILL.md") },
            Requested { name: "flat".into(), path: src.path().join("flat.md") },
        ];
        let under = Path::new(".claude/skills");
        assert_eq!(skills.link(under, &want), ["rex", "flat"]);
        assert_eq!(skills.link(under, &want), Vec::<String>::new(), "once");
        let gone = [Requested { name: "gone".into(), path: src.path().join("gone/SKILL.md") }];
        assert!(skills.link(under, &gone).is_empty(), "nothing to link to");
        assert_eq!(std::fs::read_to_string(skills.skill_file(under, "rex")).unwrap(), "rex");
        assert_eq!(std::fs::read_to_string(skills.skill_file(under, "flat")).unwrap(), "flat");
        drop(skills);
        assert!(!root.exists());
        assert!(src.path().join("rex/SKILL.md").exists() && src.path().join("flat.md").exists(), "the skills themselves stay");
    }
}
