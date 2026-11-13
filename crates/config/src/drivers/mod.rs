//! Skill install drivers: put discovered skills where each harness expects them and link
//! `AXEL.md` as the harness's context file.
//!
//! Ported from Axel v1 `drivers/*` keeping only install / cleanup / context; OTEL,
//! tmux bell hooks and the Antigravity driver were dropped.
//!
//! Every driver works with symlinks and never clobbers a regular file or directory it
//! did not create: an existing non-symlink at a target path is left alone.

mod claude;
mod codex;
mod opencode;

use std::{
    fs,
    path::{Path, PathBuf},
};

pub use claude::ClaudeDriver;
pub use codex::CodexDriver;
pub use opencode::OpenCodeDriver;

use crate::{Error, HarnessKind, Skill};

/// Installs skills and project context for one harness.
pub trait SkillDriver {
    /// Symlink `skills` into the harness's skill location under `project`.
    fn install(&self, project: &Path, skills: &[Skill]) -> Result<(), Error>;
    /// Remove the symlinks a previous [`SkillDriver::install`] / [`SkillDriver::install_context`] created.
    fn cleanup(&self, project: &Path) -> Result<(), Error>;
    /// Link `axel_md` as the harness's context file (`CLAUDE.md`, `AGENTS.md`, ...).
    ///
    /// The link is relative when `axel_md` sits in `project` (`CLAUDE.md -> AXEL.md`),
    /// so it still works in a clone. `Ok(false)` when the project already has its own
    /// context file, which is left alone: the harness then won't read `AXEL.md`.
    fn install_context(&self, project: &Path, axel_md: &Path) -> Result<bool, Error> {
        let link = project.join(self.context_filename());
        let beside = canonical(axel_md).parent() == Some(canonical(project).as_path());
        match axel_md.file_name() {
            Some(name) if beside => place_link(Path::new(name), &link),
            _ => place_symlink(axel_md, &link),
        }
    }

    /// Harness name (`claude`, `codex`, `opencode`).
    fn name(&self) -> &'static str;
    /// Directory skills are installed into.
    fn skills_dir(&self, project: &Path) -> PathBuf;
    /// Project context file the harness reads.
    fn context_filename(&self) -> &'static str;
}

/// The driver for a harness kind. `None` for `Custom`.
pub fn for_harness(kind: HarnessKind) -> Option<Box<dyn SkillDriver>> {
    match kind {
        HarnessKind::Claude => Some(Box::new(ClaudeDriver)),
        HarnessKind::Codex => Some(Box::new(CodexDriver)),
        HarnessKind::Opencode => Some(Box::new(OpenCodeDriver)),
        HarnessKind::Custom => None,
    }
}

/// Every builtin driver.
pub fn all() -> Vec<Box<dyn SkillDriver>> {
    HarnessKind::BUILTIN
        .iter()
        .filter_map(|k| for_harness(*k))
        .collect()
}

// -----------------------------------------------------------------------------
// shared helpers
// -----------------------------------------------------------------------------

pub(crate) fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<(), Error> {
    std::os::unix::fs::symlink(target, link).map_err(|e| Error::io(link, e))
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> Result<(), Error> {
    Err(Error::SymlinkUnsupported)
}

/// Make `link` a symlink to `target`. Replaces an existing symlink; returns `Ok(false)`
/// without touching anything when a regular file or directory is already there.
pub(crate) fn place_symlink(target: &Path, link: &Path) -> Result<bool, Error> {
    place_link(&canonical(target), link)
}

/// [`place_symlink`] with `target` used as written (it may be relative to `link`).
fn place_link(target: &Path, link: &Path) -> Result<bool, Error> {
    let target = target.to_path_buf();
    if is_symlink(link) {
        if fs::read_link(link).is_ok_and(|current| current == target) {
            return Ok(true);
        }
        fs::remove_file(link).map_err(|e| Error::io(link, e))?;
    } else if link.exists() {
        return Ok(false);
    }
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    symlink(&target, link)?;
    Ok(true)
}

/// Remove `path` if it is a symlink. Returns whether something was removed.
pub(crate) fn remove_symlink(path: &Path) -> Result<bool, Error> {
    if !is_symlink(path) {
        return Ok(false);
    }
    fs::remove_file(path).map_err(|e| Error::io(path, e))?;
    Ok(true)
}

/// Install skills in the Claude Code and Codex layout, `<dir>/<name>/SKILL.md`. A
/// directory skill is linked whole (`<dir>/<name> -> its directory`), so the scripts
/// and references beside its `SKILL.md` come along; a flat `<name>.md` skill gets a
/// `<dir>/<name>/SKILL.md` link to the file.
pub(crate) fn install_nested(dir: &Path, skills: &[Skill]) -> Result<(), Error> {
    for skill in skills {
        let slot = dir.join(&skill.name);
        let is_dir_skill = skill.path.file_name().is_some_and(|n| n == crate::skills::SKILL_FILE);
        match skill.path.parent().filter(|_| is_dir_skill) {
            Some(skill_dir) => {
                // An earlier install linked only the file: replace that with the dir link.
                if !is_symlink(&slot) && remove_symlink(&slot.join(crate::skills::SKILL_FILE))? {
                    let _ = fs::remove_dir(&slot);
                }
                if let Some(parent) = slot.parent() {
                    fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
                }
                place_symlink(skill_dir, &slot)?;
            }
            None => {
                place_symlink(&skill.path, &slot.join(crate::skills::SKILL_FILE))?;
            }
        }
    }
    Ok(())
}

/// Undo [`install_nested`]: remove its links and the directories they leave empty.
pub(crate) fn cleanup_nested(dir: &Path) -> Result<(), Error> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(|e| Error::io(dir, e))? {
        let path = entry.map_err(|e| Error::io(dir, e))?.path();
        if remove_symlink(&path)? {
            continue;
        }
        if path.is_dir() && remove_symlink(&path.join(crate::skills::SKILL_FILE))? {
            let _ = fs::remove_dir(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(dir: &Path, name: &str) -> Skill {
        let path = dir.join(format!("{name}.md"));
        fs::write(&path, format!("# {name}\n")).unwrap();
        Skill::from_file(&path).unwrap()
    }

    #[test]
    fn claude_and_codex_install_nested_and_clean_up() {
        let root = tempfile::tempdir().unwrap();
        let skills_src = root.path().join("skills");
        fs::create_dir_all(&skills_src).unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let axel_md = project.join("AXEL.md");
        fs::write(&axel_md, "# ctx\n").unwrap();
        let skills = vec![skill(&skills_src, "alpha"), skill(&skills_src, "beta")];

        for (driver, skills_dir, ctx) in [
            (
                for_harness(HarnessKind::Claude).unwrap(),
                ".claude/skills",
                "CLAUDE.md",
            ),
            (
                for_harness(HarnessKind::Codex).unwrap(),
                ".codex/skills",
                "AGENTS.md",
            ),
        ] {
            driver.install(&project, &skills).unwrap();
            driver.install_context(&project, &axel_md).unwrap();
            let link = project.join(skills_dir).join("alpha").join("SKILL.md");
            assert!(is_symlink(&link));
            assert_eq!(
                fs::read_link(&link).unwrap(),
                skills_src.join("alpha.md").canonicalize().unwrap()
            );
            assert_eq!(fs::read_to_string(project.join(ctx)).unwrap(), "# ctx\n");
            assert!(is_symlink(&project.join(ctx)));
            assert_eq!(fs::read_link(project.join(ctx)).unwrap(), PathBuf::from("AXEL.md"), "relative, so clones work");

            // idempotent
            driver.install(&project, &skills).unwrap();
            driver.install_context(&project, &axel_md).unwrap();

            driver.cleanup(&project).unwrap();
            assert!(!project.join(skills_dir).join("alpha").exists());
            assert!(!project.join(ctx).exists());
            assert!(axel_md.is_file());
        }
    }

    #[test]
    fn directory_skills_are_linked_whole_and_real_context_files_kept() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("remote/review");
        fs::create_dir_all(src.join("scripts")).unwrap();
        fs::write(src.join(crate::skills::SKILL_FILE), "# review\n").unwrap();
        fs::write(src.join("scripts/run.sh"), "echo\n").unwrap();
        let review = Skill::from_file(&src.join(crate::skills::SKILL_FILE)).unwrap();
        let project = root.path().join("project");
        // An install from before linked only the file.
        let slot = project.join(".claude/skills/review");
        fs::create_dir_all(&slot).unwrap();
        symlink(&src.join(crate::skills::SKILL_FILE), &slot.join(crate::skills::SKILL_FILE)).unwrap();

        let driver = for_harness(HarnessKind::Claude).unwrap();
        driver.install(&project, std::slice::from_ref(&review)).unwrap();
        assert!(is_symlink(&slot));
        assert!(slot.join("scripts/run.sh").is_file(), "the skill's files come along");
        driver.cleanup(&project).unwrap();
        assert!(!slot.exists() && src.join("scripts/run.sh").is_file());

        fs::write(project.join("CLAUDE.md"), "# mine\n").unwrap();
        fs::write(project.join("AXEL.md"), "# axel\n").unwrap();
        assert!(!driver.install_context(&project, &project.join("AXEL.md")).unwrap());
        assert_eq!(fs::read_to_string(project.join("CLAUDE.md")).unwrap(), "# mine\n");
    }

    #[test]
    fn opencode_installs_flat_files() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let axel_md = project.join("AXEL.md");
        fs::write(&axel_md, "# ctx\n").unwrap();
        let skills = vec![skill(root.path(), "alpha")];

        let driver = for_harness(HarnessKind::Opencode).unwrap();
        driver.install(&project, &skills).unwrap();
        driver.install_context(&project, &axel_md).unwrap();
        assert!(is_symlink(&project.join(".opencode/skill/alpha.md")));
        assert!(is_symlink(&project.join("AGENTS.md")));
        driver.cleanup(&project).unwrap();
        assert!(!project.join(".opencode/skill/alpha.md").exists());
        assert!(!project.join("AGENTS.md").exists());
    }

    #[test]
    fn never_clobbers_regular_files() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(project.join(".claude/skills/alpha")).unwrap();
        fs::write(project.join(".claude/skills/alpha/SKILL.md"), "mine").unwrap();
        fs::write(project.join("CLAUDE.md"), "my own context").unwrap();
        let axel_md = project.join("AXEL.md");
        fs::write(&axel_md, "# ctx\n").unwrap();
        let skills = vec![skill(root.path(), "alpha")];

        let driver = ClaudeDriver;
        driver.install(&project, &skills).unwrap();
        driver.install_context(&project, &axel_md).unwrap();
        assert_eq!(
            fs::read_to_string(project.join(".claude/skills/alpha/SKILL.md")).unwrap(),
            "mine"
        );
        assert_eq!(
            fs::read_to_string(project.join("CLAUDE.md")).unwrap(),
            "my own context"
        );

        driver.cleanup(&project).unwrap();
        assert!(project.join(".claude/skills/alpha/SKILL.md").is_file());
        assert!(project.join("CLAUDE.md").is_file());
    }

    #[test]
    fn custom_has_no_driver() {
        assert!(for_harness(HarnessKind::Custom).is_none());
        assert_eq!(all().len(), 3);
    }
}
