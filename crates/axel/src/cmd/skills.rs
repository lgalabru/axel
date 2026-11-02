//! `axel skills`: what skills a project can use, and moving remote skills' pins.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use axel_config::{WorkspaceConfig, remote};

use crate::cli::SkillsCommand;
use crate::paths;

pub fn run(command: SkillsCommand) -> Result<()> {
    match command {
        SkillsCommand::Ls => ls(),
        SkillsCommand::Update { source, yes } => update(source.as_deref(), yes),
    }
}

/// The nearest `AXEL.md`, from the current directory up to the repository root.
fn manifest() -> Result<Option<WorkspaceConfig>> {
    let cwd = std::env::current_dir()?;
    for dir in cwd.ancestors() {
        if let Some(config) = WorkspaceConfig::load(dir).context("reading AXEL.md")? {
            return Ok(Some(config));
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    Ok(None)
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(7)]
}

fn tilde(p: &Path) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(&h).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
}

/// Where a discovered skill comes from, for `ls`.
fn origin(skill: &Path, project: Option<&Path>) -> String {
    let cache = paths::skills_cache_dir().join("github");
    if let Ok(rest) = skill.strip_prefix(&cache) {
        let parts: Vec<String> = rest.components().take(3).map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        if let [owner, repo, commit] = parts.as_slice() {
            return format!("{owner}/{repo} @ {}", short(commit));
        }
    }
    if skill.starts_with(paths::global_skills_dir()) {
        return format!("global  {}", tilde(skill.parent().unwrap_or(skill)));
    }
    match project.and_then(|p| skill.strip_prefix(p).ok()) {
        Some(rel) => format!("./{}", rel.display()),
        None => tilde(skill),
    }
}

fn ls() -> Result<()> {
    let config = manifest()?;
    let all = super::launch::project_skills(config.as_ref())?;
    if all.is_empty() {
        println!("no skills here. Add sources under `skills:` in AXEL.md (a directory, or a skills.sh link).");
        return Ok(());
    }
    let project = config.as_ref().and_then(|c| c.dir()).map(Path::to_path_buf);
    let width = all.keys().map(String::len).max().unwrap_or(0);
    for (name, skill) in &all {
        println!("{name:<width$}  {}", origin(&skill.path, project.as_deref()));
        if let Some(d) = &skill.description {
            let d: String = d.chars().take(96).collect();
            println!("{:<width$}  \x1b[2m{d}\x1b[0m", "");
        }
    }
    Ok(())
}

/// `source` as typed matches `g`: its key, its key without the ref, or a spelling that
/// parses to the same source.
fn matches(source: &str, g: &axel_config::GitHubSkills) -> bool {
    let key = g.key();
    source == key
        || key.split('@').next() == Some(source)
        || axel_config::SkillSource::parse(source, None).is_ok_and(|s| s == axel_config::SkillSource::GitHub(g.clone()))
}

fn update(only: Option<&str>, yes: bool) -> Result<()> {
    let Some(config) = manifest()? else {
        bail!("no AXEL.md here or above (up to the repository root)");
    };
    let dir = config.dir().map(Path::to_path_buf).context("AXEL.md has no directory")?;
    if config.remote_skills().is_empty() {
        println!("AXEL.md lists no remote skills.");
        return Ok(());
    }
    if let Some(only) = only
        && !config.remote_skills().iter().any(|g| matches(only, g))
    {
        bail!("AXEL.md has no remote skill source `{only}`");
    }
    eprintln!("checking {} remote source(s)…", config.remote_skills().len());
    let (mut lock, updates, errors) = remote::check_updates(&config, &paths::skills_cache_dir());
    for e in &errors {
        eprintln!("axel: {e}");
    }
    let updates: Vec<_> = updates.into_iter().filter(|u| only.is_none_or(|o| matches(o, &u.source))).collect();
    if updates.is_empty() {
        println!("every pin is current.");
        return Ok(());
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let mut changed = false;
    for u in &updates {
        let from = u.from.as_deref().map(short).unwrap_or("unpinned");
        println!("\n\x1b[1m{}\x1b[0m  {from} → {}", u.source, short(&u.to));
        show_diff(&u.old, &u.new);
        let apply = if yes {
            true
        } else if interactive {
            dialoguer::Confirm::new().with_prompt(format!("Pin {} to {}?", u.source, short(&u.to))).default(false).interact()?
        } else {
            false
        };
        if apply {
            lock.skills.insert(u.source.key(), u.to.clone());
            changed = true;
        }
    }
    if changed {
        lock.save(&dir)?;
        println!("\nwrote {}", tilde(&dir.join(remote::AXEL_LOCK)));
    } else if !interactive && !yes {
        println!("\nnothing applied; rerun with --yes to move these pins.");
    }
    Ok(())
}

/// What changed in each skill directory, as `git diff` shows it.
fn show_diff(old: &[PathBuf], new: &[PathBuf]) {
    let color = if std::io::stdout().is_terminal() { "--color=always" } else { "--color=never" };
    for new_dir in new {
        let name = new_dir.file_name().unwrap_or_default();
        let Some(old_dir) = old.iter().find(|o| o.file_name() == Some(name)) else {
            println!("  new skill: {}", name.to_string_lossy());
            continue;
        };
        // `git diff --no-index` exits 1 when the trees differ; that's the point.
        let _ = Command::new("git").args(["diff", "--no-index", "--stat", color]).arg(old_dir).arg(new_dir).status();
        let _ = Command::new("git").args(["diff", "--no-index", color]).arg(old_dir).arg(new_dir).status();
    }
    for old_dir in old {
        if !new.iter().any(|n| n.file_name() == old_dir.file_name()) {
            println!("  removed upstream: {}", old_dir.file_name().unwrap_or_default().to_string_lossy());
        }
    }
}
