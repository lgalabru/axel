//! Machine-local Claude account selection for an Axel agent.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use axel_store::Harness;

const CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";
const AUTH_OVERRIDES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_VERTEX",
];

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ClaudeProfile {
    Default,
    Directory(PathBuf),
}

impl ClaudeProfile {
    pub fn parse(value: &str, harness: &Harness) -> Result<Self> {
        if *harness != Harness::Claude {
            bail!("--claude-config-dir requires the Claude harness");
        }
        if value == "default" {
            return Ok(Self::Default);
        }
        let path = Path::new(value);
        if !path.is_absolute() || !path.is_dir() {
            bail!("use an existing absolute Claude config directory or 'default'");
        }
        Ok(Self::Directory(path.to_path_buf()))
    }

    fn path(agent: &str) -> Result<PathBuf> {
        let key: axel_identity::PublicKey = agent.parse().context("invalid agent public key")?;
        Ok(crate::paths::home()
            .join("claude-profiles")
            .join(key.to_string()))
    }

    pub fn load(agent: &str) -> Result<Option<Self>> {
        let Ok(path) = Self::path(agent) else {
            return Ok(None);
        };
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("reading the agent's Claude profile")?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading the agent's Claude profile"),
        }
    }

    pub fn save(&self, agent: &str) -> Result<()> {
        let path = Self::path(agent)?;
        let dir = path.parent().context("missing Claude profile directory")?;
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(tmp.as_file_mut(), self)?;
        tmp.persist(path)?;
        Ok(())
    }

    pub fn clear(agent: &str) -> Result<()> {
        match std::fs::remove_file(Self::path(agent)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("removing the agent's Claude profile"),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Default => "default",
            Self::Directory(path) => path.to_str().unwrap_or("<non-UTF-8 path>"),
        }
    }

    pub fn state_path(&self) -> Result<PathBuf> {
        match self {
            Self::Default => super::launch::default_claude_state_path(),
            Self::Directory(path) if path.is_dir() && path.is_absolute() => {
                Ok(path.join(".claude.json"))
            }
            Self::Directory(path) => {
                bail!("Claude config directory is unavailable: {}", path.display())
            }
        }
    }

    /// Set the environment before starting the host's async runtime.
    pub fn configure_host(&self) -> Result<()> {
        self.state_path()?;
        // SAFETY: session::host calls this before spawning threads.
        unsafe {
            for key in AUTH_OVERRIDES.iter().copied().chain([CONFIG_DIR]) {
                std::env::remove_var(key);
            }
            if let Self::Directory(path) = self {
                std::env::set_var(CONFIG_DIR, path);
            }
        }
        Ok(())
    }

    pub fn overrides(&self, key: &str) -> bool {
        key == CONFIG_DIR || AUTH_OVERRIDES.contains(&key)
    }

    /// Apply the profile to an interactive pane.
    /// `env -u` also works when Rex starts the command directly.
    pub fn wrap(&self, program: &mut String, args: &mut Vec<String>) -> Result<()> {
        self.state_path()?;
        let mut prefix = Vec::new();
        for key in AUTH_OVERRIDES.iter().copied().chain([CONFIG_DIR]) {
            prefix.extend(["-u".into(), key.into()]);
        }
        if let Self::Directory(path) = self {
            let value = path
                .to_str()
                .context("Claude config directory must be UTF-8")?;
            prefix.push(format!("{CONFIG_DIR}={value}"));
        }
        prefix.push(std::mem::replace(program, "/usr/bin/env".into()));
        prefix.append(args);
        *args = prefix;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn a_profile_selects_its_directory_and_ignores_inherited_auth_overrides() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("my Claude");
        std::fs::create_dir(&path).unwrap();
        let profile = ClaudeProfile::parse(path.to_str().unwrap(), &Harness::Claude).unwrap();
        for (profile, expected) in [
            (profile, path.to_str().unwrap()),
            (ClaudeProfile::Default, ""),
        ] {
            let mut program = "/bin/sh".into();
            let mut args = vec![
                "-c".into(),
                "printf '%s\n%s' \"${CLAUDE_CONFIG_DIR-}\" \"${ANTHROPIC_API_KEY-}\"".into(),
            ];
            profile.wrap(&mut program, &mut args).unwrap();
            let result = Command::new(program)
                .args(args)
                .env(CONFIG_DIR, "/wrong")
                .env("ANTHROPIC_API_KEY", "test")
                .output()
                .unwrap();
            assert!(result.status.success());
            assert_eq!(
                String::from_utf8(result.stdout).unwrap(),
                format!("{expected}\n")
            );
        }
    }
}
