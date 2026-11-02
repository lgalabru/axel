//! Well-known paths under `~/.axel`.

use std::path::PathBuf;

pub fn home() -> PathBuf {
    if let Ok(p) = std::env::var("AXEL_HOME") {
        return PathBuf::from(p);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".axel")
}

pub fn hooks_dir() -> PathBuf {
    home().join("hooks")
}

pub fn worktrees_dir() -> PathBuf {
    home().join("worktrees")
}

/// `~/.axel/skills/cache`: remote skills, one checkout per commit.
pub fn skills_cache_dir() -> PathBuf {
    home().join("skills").join("cache")
}

pub fn global_skills_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(home)
        .join("axel")
        .join("skills")
}
