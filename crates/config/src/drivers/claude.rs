//! Claude Code: `.claude/skills/<name>/SKILL.md` symlinks (each becomes `/name`) and
//! `CLAUDE.md` -> `AXEL.md`.

use std::path::{Path, PathBuf};

use super::{SkillDriver, cleanup_nested, install_nested, remove_symlink};
use crate::{Error, Skill};

pub struct ClaudeDriver;

impl SkillDriver for ClaudeDriver {
    fn install(&self, project: &Path, skills: &[Skill]) -> Result<(), Error> {
        install_nested(&self.skills_dir(project), skills)
    }

    fn cleanup(&self, project: &Path) -> Result<(), Error> {
        cleanup_nested(&self.skills_dir(project))?;
        remove_symlink(&project.join(self.context_filename()))?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "claude"
    }

    fn skills_dir(&self, project: &Path) -> PathBuf {
        project.join(".claude").join("skills")
    }

    fn context_filename(&self) -> &'static str {
        "CLAUDE.md"
    }
}
