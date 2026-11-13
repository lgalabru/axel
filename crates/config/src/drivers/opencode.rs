//! OpenCode: `.opencode/skill/<name>.md` symlinks and `AGENTS.md` -> `AXEL.md`.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{SkillDriver, place_symlink, remove_symlink};
use crate::{Error, Skill};

pub struct OpenCodeDriver;

impl SkillDriver for OpenCodeDriver {
    fn install(&self, project: &Path, skills: &[Skill]) -> Result<(), Error> {
        let dir = self.skills_dir(project);
        for skill in skills {
            place_symlink(&skill.path, &dir.join(format!("{}.md", skill.name)))?;
        }
        Ok(())
    }

    fn cleanup(&self, project: &Path) -> Result<(), Error> {
        let dir = self.skills_dir(project);
        if dir.is_dir() {
            for entry in fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))? {
                let path = entry.map_err(|e| Error::io(&dir, e))?.path();
                if path.extension().is_some_and(|ext| ext == "md") {
                    remove_symlink(&path)?;
                }
            }
        }
        remove_symlink(&project.join(self.context_filename()))?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "opencode"
    }

    fn skills_dir(&self, project: &Path) -> PathBuf {
        project.join(".opencode").join("skill")
    }

    fn context_filename(&self) -> &'static str {
        "AGENTS.md"
    }
}
