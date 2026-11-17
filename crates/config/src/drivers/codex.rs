//! Codex: `.codex/skills/<name>/SKILL.md` symlinks (discoverable via `/skills`) and
//! `AGENTS.md` -> `AXEL.md`. The launcher additionally passes
//! `-c project_doc_fallback_filenames=["AXEL.md"]`, so the link is belt and braces.

use std::path::{Path, PathBuf};

use super::{SkillDriver, cleanup_nested, install_nested, remove_symlink};
use crate::{Error, Skill};

pub struct CodexDriver;

impl SkillDriver for CodexDriver {
    fn install(&self, project: &Path, skills: &[Skill]) -> Result<(), Error> {
        install_nested(&self.skills_dir(project), skills)
    }

    fn cleanup(&self, project: &Path) -> Result<(), Error> {
        cleanup_nested(&self.skills_dir(project))?;
        remove_symlink(&project.join(self.context_filename()))?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "codex"
    }

    fn skills_dir(&self, project: &Path) -> PathBuf {
        project.join(".codex").join("skills")
    }

    fn context_filename(&self) -> &'static str {
        "AGENTS.md"
    }

    /// Where Axel installs, then the shared `.agents/skills` Codex reads.
    fn project_skill_dirs(&self, project: &Path) -> Vec<PathBuf> {
        vec![self.skills_dir(project), project.join(".agents").join("skills")]
    }

    /// The user's `~/.agents/skills`, then Codex's own `~/.codex/skills`.
    fn user_skill_dirs(&self, home: &Path) -> Vec<PathBuf> {
        vec![home.join(".agents").join("skills"), home.join(".codex").join("skills")]
    }

}
