//! Skill discovery: `skills/<name>.md` or `skills/<name>/SKILL.md` with optional YAML
//! frontmatter (`name`, `description`). Ported from Axel v1 `config.rs`.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{Error, SkillSource, config::expand_tilde, config::split_frontmatter, remote::SkillRoot};

/// File name for the directory form of a skill.
pub const SKILL_FILE: &str = "SKILL.md";

/// A discovered skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Skill {
    /// Frontmatter `name`, or the file stem / directory name.
    pub name: String,
    /// The markdown file (`<name>.md` or `<name>/SKILL.md`).
    pub path: PathBuf,
    /// Frontmatter `description`, or the first non-heading line of the body.
    pub description: Option<String>,
}

#[derive(Deserialize, Default)]
struct SkillFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

impl Skill {
    /// Name implied by a skill path: the parent directory for `SKILL.md`, otherwise the
    /// file stem.
    pub fn derive_name(path: &Path) -> String {
        let from_dir = path.file_name().is_some_and(|n| n == SKILL_FILE);
        let name = if from_dir {
            path.parent().and_then(Path::file_name)
        } else {
            path.file_stem()
        };
        name.map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "skill".to_string())
    }

    /// Parse a skill file. Malformed frontmatter is treated as plain body.
    pub fn from_file(path: &Path) -> Result<Skill, Error> {
        let text = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let (yaml, body) = split_frontmatter(&text).unwrap_or((None, text.as_str()));
        let frontmatter = yaml
            .filter(|y| !y.trim().is_empty())
            .and_then(|y| serde_norway::from_str::<SkillFrontmatter>(y).ok())
            .unwrap_or_default();

        let name = frontmatter
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| Self::derive_name(path));
        let description = frontmatter
            .description
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .or_else(|| summary_line(body));

        Ok(Skill {
            name,
            path: path.to_path_buf(),
            description,
        })
    }

    /// The skill's markdown with frontmatter removed.
    pub fn body(&self) -> Result<String, Error> {
        let text = fs::read_to_string(&self.path).map_err(|e| Error::io(&self.path, e))?;
        let (_, body) = split_frontmatter(&text).unwrap_or((None, text.as_str()));
        Ok(body.trim().to_string())
    }
}

/// First non-empty, non-heading line; falls back to the first heading's text.
fn summary_line(body: &str) -> Option<String> {
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    lines
        .iter()
        .find(|l| !l.starts_with('#'))
        .or_else(|| lines.first())
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|l| !l.is_empty())
}

/// `~/.config/axel/skills`, the conventional machine-wide skill directory.
pub fn default_global_skills_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".config").join("axel").join("skills"))
}

/// Discover every skill in the directory sources (in order), then `global`. Remote
/// sources are skipped: resolve them with [`crate::remote::resolve`] and use
/// [`find_skills`].
pub fn find_all_skills(
    sources: &[SkillSource],
    global: Option<&Path>,
) -> Result<IndexMap<String, Skill>, Error> {
    let roots: Vec<SkillRoot> = sources.iter().filter_map(|s| s.resolve(None)).map(SkillRoot::Dir).collect();
    find_skills(&roots, global)
}

/// Discover every skill under `roots` and `global`.
///
/// Directories on disk come first (in order), then remote skills, then `global`: the
/// first to define a name wins and later duplicates are ignored, so a local copy of a
/// remote skill overrides it. Keys are [`Skill::name`]. Directories that do not exist
/// are skipped. Within a directory, `index.md` and `README.md` are not skills, nor is
/// a bare `SKILL.md`.
pub fn find_skills(roots: &[SkillRoot], global: Option<&Path>) -> Result<IndexMap<String, Skill>, Error> {
    let mut files = Vec::new();
    for root in roots {
        if let SkillRoot::Dir(dir) = root
            && dir.is_dir()
        {
            files.extend(skill_files_in(dir)?);
        }
    }
    for root in roots {
        if let SkillRoot::Files(f) = root {
            files.extend(f.iter().cloned());
        }
    }
    if let Some(global) = global.map(expand_tilde).filter(|g| g.is_dir()) {
        files.extend(skill_files_in(&global)?);
    }
    let mut skills = IndexMap::new();
    for path in files {
        let skill = Skill::from_file(&path)?;
        skills.entry(skill.name.clone()).or_insert(skill);
    }
    Ok(skills)
}

/// Skill files directly under `dir`, sorted by entry name for determinism.
fn skill_files_in(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| Error::io(dir, e))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .map_err(|e| Error::io(dir, e))?;
    entries.sort();

    let mut files = Vec::new();
    for path in entries {
        if path.is_dir() {
            let file = path.join(SKILL_FILE);
            if file.is_file() {
                files.push(file);
            }
        } else if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if ["index.md", "readme.md", "skill.md"]
                .contains(&file_name.to_ascii_lowercase().as_str())
            {
                continue;
            }
            files.push(path);
        }
    }
    Ok(files)
}

/// Pick the requested skills out of `all`, preserving request order and dropping
/// duplicates. A request containing `"*"` selects every skill in discovery order.
/// Unknown names are skipped; see [`missing_skills`].
pub fn resolve_skills(requested: &[String], all: &IndexMap<String, Skill>) -> Vec<Skill> {
    if requested.iter().any(|r| r == "*") {
        return all.values().cloned().collect();
    }
    let mut seen = HashSet::new();
    requested
        .iter()
        .filter(|name| seen.insert(name.as_str()))
        .filter_map(|name| all.get(name.as_str()).cloned())
        .collect()
}

/// Requested names (other than `"*"`) that are not in `all`.
pub fn missing_skills(requested: &[String], all: &IndexMap<String, Skill>) -> Vec<String> {
    requested
        .iter()
        .filter(|name| *name != "*" && !all.contains_key(name.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn parses_flat_and_directory_skills() {
        let dir = tempfile::tempdir().unwrap();
        let flat = dir.path().join("review.md");
        write(&flat, "# Reviewer\n\nReview code carefully.\n");
        let skill = Skill::from_file(&flat).unwrap();
        assert_eq!(skill.name, "review");
        assert_eq!(skill.description.as_deref(), Some("Review code carefully."));

        let nested = dir.path().join("deploy").join(SKILL_FILE);
        write(
            &nested,
            "---\nname: deployer\ndescription: Ship it\n---\n\n# Deploy\n",
        );
        let skill = Skill::from_file(&nested).unwrap();
        assert_eq!(skill.name, "deployer");
        assert_eq!(skill.description.as_deref(), Some("Ship it"));
        assert_eq!(skill.body().unwrap(), "# Deploy");

        let heading_only = dir.path().join("h.md");
        write(&heading_only, "# Only Heading\n");
        assert_eq!(
            Skill::from_file(&heading_only)
                .unwrap()
                .description
                .as_deref(),
            Some("Only Heading")
        );

        let broken = dir.path().join("broken.md");
        write(&broken, "---\n: [not yaml\n---\nbody line\n");
        let skill = Skill::from_file(&broken).unwrap();
        assert_eq!(skill.name, "broken");
        assert_eq!(skill.description.as_deref(), Some("body line"));
    }

    #[test]
    fn first_directory_wins_and_star_selects_all() {
        let root = tempfile::tempdir().unwrap();
        let local = root.path().join("local");
        let global = root.path().join("global");
        write(&local.join("alpha.md"), "local alpha");
        write(&local.join("beta").join(SKILL_FILE), "local beta");
        write(&local.join("index.md"), "not a skill");
        write(&local.join("README.md"), "not a skill either");
        write(&global.join("alpha.md"), "global alpha");
        write(&global.join("gamma.md"), "global gamma");

        let sources = [
            SkillSource::new(&local),
            SkillSource::new(root.path().join("missing")),
        ];
        let all = find_all_skills(&sources, Some(&global)).unwrap();
        let names: Vec<&str> = all.keys().map(String::as_str).collect();
        assert_eq!(names, ["alpha", "beta", "gamma"]);
        assert_eq!(all["alpha"].path, local.join("alpha.md"));
        assert_eq!(all["gamma"].path, global.join("gamma.md"));

        let star = resolve_skills(&["*".to_string()], &all);
        assert_eq!(star.len(), 3);

        let some = resolve_skills(
            &[
                "gamma".into(),
                "alpha".into(),
                "gamma".into(),
                "nope".into(),
            ],
            &all,
        );
        let picked: Vec<&str> = some.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(picked, ["gamma", "alpha"]);
        assert_eq!(
            missing_skills(&["gamma".into(), "nope".into(), "*".into()], &all),
            ["nope"]
        );

        assert!(find_all_skills(&[], None).unwrap().is_empty());
    }
}
