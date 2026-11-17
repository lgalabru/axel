//! Skill discovery: `skills/<name>.md` or `skills/<name>/SKILL.md` with optional YAML
//! frontmatter (`name`, `description`). Ported from Axel v1 `config.rs`.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{Error, SkillSource, config::expand_tilde, config::read_text, config::split_frontmatter, remote::SkillRoot};

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
        let text = read_text(path).map_err(|e| Error::io(path, e))?;
        // Unterminated or unparsable frontmatter isn't an error here: the skill is
        // still usable, named after its path and described by its body.
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
        let text = read_text(&self.path).map_err(|e| Error::io(&self.path, e))?;
        // As in `from_file`: frontmatter that doesn't close is part of the body.
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

/// Where a hinted skill was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum SkillScope {
    /// In one of the thread's work directories.
    Project,
    /// In the user's home directory, for every project.
    User,
}

/// A skill the composer can suggest by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SkillHint {
    pub name: String,
    pub description: Option<String>,
    pub scope: SkillScope,
    /// Its markdown file: what an agent reads when its harness doesn't load it.
    pub path: PathBuf,
}

/// Every skill directory any harness loads, for `work_dirs`: each driver's
/// project directories in each work directory, then each driver's user
/// directories under `home`, then Axel's own `global` skills. Each once.
pub fn all_skill_dirs(work_dirs: &[PathBuf], home: &Path, global: Option<&Path>) -> Vec<(PathBuf, SkillScope)> {
    let drivers = crate::drivers::all();
    let mut dirs: Vec<(PathBuf, SkillScope)> = Vec::new();
    let mut add = |d: PathBuf, scope| {
        if !dirs.iter().any(|(have, _)| *have == d) {
            dirs.push((d, scope));
        }
    };
    for dir in work_dirs {
        for driver in &drivers {
            driver.project_skill_dirs(dir).into_iter().for_each(|d| add(d, SkillScope::Project));
        }
    }
    for driver in &drivers {
        driver.user_skill_dirs(home).into_iter().for_each(|d| add(d, SkillScope::User));
    }
    if let Some(g) = global {
        add(expand_tilde(g), SkillScope::User);
    }
    dirs
}

/// The `/name` words in `text`: a `/` that starts a word, then a name
/// (letters, digits, `_ - : .`, trailing `.` and `:` left out as
/// punctuation), not followed by another `/` (that's a path). Each as the
/// char range of the whole word and the name.
fn skill_words(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '/' && (i == 0 || chars[i - 1].is_whitespace()) {
            let mut end = i + 1;
            while end < chars.len() && (chars[end].is_alphanumeric() || matches!(chars[end], '_' | '-' | ':' | '.')) {
                end += 1;
            }
            let path = chars.get(end) == Some(&'/');
            let mut name_end = end;
            while name_end > i + 1 && matches!(chars[name_end - 1], '.' | ':') {
                name_end -= 1;
            }
            if !path && name_end > i + 1 {
                out.push((i..name_end, chars[i + 1..name_end].iter().collect()));
            }
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

/// The skills in `known` that `text` names as `/name`, each once, in the
/// order named. A `/name` that names no known skill, or a path, isn't one.
pub fn named_skills<'a>(text: &str, known: &'a [SkillHint]) -> Vec<&'a SkillHint> {
    let mut out: Vec<&SkillHint> = Vec::new();
    for (_, name) in skill_words(text) {
        if let Some(s) = known.iter().find(|s| s.name == name)
            && !out.iter().any(|o| o.name == s.name)
        {
            out.push(s);
        }
    }
    out
}

/// `text` with each `/name` word for a skill in `names` spelled with
/// `sigil` instead (`$name` for Codex). Other words are left alone.
pub fn rewrite_skill_words(text: &str, names: &[&str], sigil: char) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    for (range, name) in skill_words(text) {
        if names.contains(&name.as_str()) {
            chars[range.start] = sigil;
        }
    }
    chars.into_iter().collect()
}

/// For an agent that can't be given skills (its adapter doesn't take
/// `_meta.axel.skills`): a note naming each skill's file, to read and follow.
pub fn skill_pointers(skills: &[(&str, &Path)]) -> String {
    let mut out = String::from("Skills named above that you don't have installed: read each file and follow it.");
    for (name, path) in skills {
        out.push_str(&format!("\n- /{name}: {}", path.display()));
    }
    out
}

/// The skills in `dirs`, in order, each name once (the first found wins).
/// A directory that's missing or unreadable, or a skill file that won't
/// parse, is skipped: one broken skill mustn't hide the rest.
pub fn skill_hints(dirs: &[(PathBuf, SkillScope)]) -> Vec<SkillHint> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (dir, scope) in dirs {
        let Ok(files) = skill_files_in(dir) else { continue };
        for file in files {
            let Ok(skill) = Skill::from_file(&file) else { continue };
            if seen.insert(skill.name.clone()) {
                out.push(SkillHint { name: skill.name, description: skill.description, scope: *scope, path: skill.path });
            }
        }
    }
    out
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
    fn hints_skip_what_cant_be_read_and_keep_the_first_of_a_name() {
        let root = tempfile::tempdir().unwrap();
        let (project, user) = (root.path().join("project"), root.path().join("user"));
        write(&project.join("review/SKILL.md"), "---\nname: review\ndescription: the project's\n---\n");
        write(&user.join("review/SKILL.md"), "---\nname: review\ndescription: the user's\n---\n");
        write(&user.join("rex/SKILL.md"), "---\ndescription: Drive Rex\n---\n");
        // Not UTF-8: unreadable as a skill, skipped without hiding the rest.
        fs::create_dir_all(user.join("broken")).unwrap();
        fs::write(user.join("broken/SKILL.md"), [0xff, 0xfe, 0x00]).unwrap();
        let dirs = [(project, SkillScope::Project), (root.path().join("missing"), SkillScope::User), (user.clone(), SkillScope::User)];
        let got: Vec<(String, Option<String>, SkillScope)> = skill_hints(&dirs).into_iter().map(|h| (h.name, h.description, h.scope)).collect();
        assert_eq!(got, [
            ("review".into(), Some("the project's".into()), SkillScope::Project),
            ("rex".into(), Some("Drive Rex".into()), SkillScope::User),
        ]);
        assert_eq!(skill_hints(&[(user.clone(), SkillScope::User)])[1].path, user.join("rex/SKILL.md"));
    }

    #[test]
    fn skill_words_are_named_once_rewritten_or_pointed_at() {
        let hint = |n: &str| SkillHint { name: n.into(), description: None, scope: SkillScope::User, path: PathBuf::from(format!("/skills/{n}/SKILL.md")) };
        let known = [hint("rex"), hint("review"), hint("writing:plain-english")];
        let text = "/rex then /review, and /review again; /writing:plain-english.";
        let names: Vec<&str> = named_skills(text, &known).iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["rex", "review", "writing:plain-english"]);
        // Not skills: unknown names, paths, a slash inside a word, a bare slash.
        assert!(named_skills("/compact see /rex/notes.md and/or /Users/dylan / x", &known).is_empty());
        assert_eq!(rewrite_skill_words(text, &["rex", "review"], '$'), "$rex then $review, and $review again; /writing:plain-english.");
        assert_eq!(rewrite_skill_words("see /rex/notes.md", &["rex"], '$'), "see /rex/notes.md");
        assert_eq!(
            skill_pointers(&[("rex", known[0].path.as_path())]),
            "Skills named above that you don't have installed: read each file and follow it.\n- /rex: /skills/rex/SKILL.md"
        );
    }

    #[test]
    fn every_harness_dir_once_then_axels_own() {
        let root = tempfile::tempdir().unwrap();
        let (work, home) = (root.path().join("work"), root.path().join("home"));
        let dirs: Vec<PathBuf> = all_skill_dirs(std::slice::from_ref(&work), &home, Some(&root.path().join("axel"))).into_iter().map(|(d, _)| d).collect();
        for want in [work.join(".claude/skills"), work.join(".agents/skills"), home.join(".claude/skills"), home.join(".agents/skills"), root.path().join("axel")] {
            assert!(dirs.contains(&want), "{want:?} in {dirs:?}");
        }
        assert_eq!(dirs.iter().collect::<HashSet<_>>().len(), dirs.len(), "each once");
        assert!(dirs.iter().position(|d| d.starts_with(&work)) < dirs.iter().position(|d| d.starts_with(&home)), "project first");
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
