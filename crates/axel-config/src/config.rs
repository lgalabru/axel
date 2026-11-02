//! `AXEL.md`: YAML frontmatter (workspace, skill sources) followed by a markdown body
//! that becomes the project context for agents.
//!
//! Agents are not declared here: they are keyed identities in the user's own store, so
//! a repo can't ship them. A legacy `agents:` block is ignored.
//!
//! Ported from Axel v1 `config.rs` with one flat [`AgentProfile`] instead of the
//! per-harness enum, and without the tmux `layouts`/`grids` section (Rex owns layout).
//! Unknown top-level keys such as a legacy `layouts:` block are ignored.

use std::{
    convert::Infallible,
    fmt, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeMap};

use crate::Error;

/// File name of the workspace manifest.
pub const AXEL_MD: &str = "AXEL.md";

// =============================================================================
// Harness
// =============================================================================

/// The program that runs an agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessKind {
    Claude,
    Codex,
    Opencode,
    Custom,
}

impl HarnessKind {
    /// The harnesses Axel knows how to drive natively.
    pub const BUILTIN: [HarnessKind; 3] = [
        HarnessKind::Claude,
        HarnessKind::Codex,
        HarnessKind::Opencode,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            HarnessKind::Claude => "claude",
            HarnessKind::Codex => "codex",
            HarnessKind::Opencode => "opencode",
            HarnessKind::Custom => "custom",
        }
    }

    /// Map a harness name to a kind. Anything that is not a builtin is `Custom`.
    pub fn parse(name: &str) -> HarnessKind {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" => HarnessKind::Claude,
            "codex" => HarnessKind::Codex,
            "opencode" => HarnessKind::Opencode,
            _ => HarnessKind::Custom,
        }
    }

    pub fn is_builtin(self) -> bool {
        self != HarnessKind::Custom
    }

    /// Default executable for a builtin harness.
    pub fn program(self) -> Option<&'static str> {
        match self {
            HarnessKind::Custom => None,
            builtin => Some(builtin.as_str()),
        }
    }
}

impl fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HarnessKind {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(HarnessKind::parse(s))
    }
}

/// How to run an agent: a builtin harness, optionally with an explicit executable,
/// or a custom command.
///
/// YAML accepts either a bare name (`harness: claude`) or an object
/// (`harness: {type: custom, command: "npm run dev", args: [...]}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Harness {
    pub kind: HarnessKind,
    /// Executable to run. `None` for a builtin means "use the harness's own binary";
    /// `None` for `Custom` means "run the user's shell".
    pub command: Option<String>,
    /// Arguments always passed to the executable, before the profile's `args`.
    pub args: Vec<String>,
}

impl Harness {
    pub fn builtin(kind: HarnessKind) -> Self {
        Harness {
            kind,
            command: None,
            args: Vec::new(),
        }
    }

    pub fn custom(command: impl Into<String>, args: Vec<String>) -> Self {
        Harness {
            kind: HarnessKind::Custom,
            command: Some(command.into()),
            args,
        }
    }

    /// Build from a bare harness name.
    ///
    /// Builtin names map to their kind. `custom` and `shell` give a `Custom` harness
    /// with no command (the user's shell); any other name is treated as the executable
    /// to run, so `harness: goose` runs `goose`.
    pub fn from_name(name: &str) -> Self {
        let kind = HarnessKind::parse(name);
        let command = match kind {
            HarnessKind::Custom => match name.trim() {
                "" | "custom" | "shell" => None,
                other => Some(other.to_string()),
            },
            _ => None,
        };
        Harness {
            kind,
            command,
            args: Vec::new(),
        }
    }
}

impl Default for Harness {
    fn default() -> Self {
        Harness::builtin(HarnessKind::Claude)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum HarnessRaw {
    Name(String),
    Detailed {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        command: Option<String>,
        #[serde(default, deserialize_with = "one_or_many")]
        args: Vec<String>,
    },
}

impl<'de> Deserialize<'de> for Harness {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match HarnessRaw::deserialize(deserializer)? {
            HarnessRaw::Name(name) => Ok(Harness::from_name(&name)),
            HarnessRaw::Detailed {
                kind,
                command,
                args,
            } => {
                let mut harness = Harness::from_name(&kind);
                if command.is_some() {
                    harness.command = command;
                }
                harness.args = args;
                Ok(harness)
            }
        }
    }
}

impl Serialize for Harness {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.kind.is_builtin() && self.command.is_none() && self.args.is_empty() {
            return serializer.serialize_str(self.kind.as_str());
        }
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("type", self.kind.as_str())?;
        if let Some(command) = &self.command {
            map.serialize_entry("command", command)?;
        }
        if !self.args.is_empty() {
            map.serialize_entry("args", &self.args)?;
        }
        map.end()
    }
}

// =============================================================================
// API + agent profile
// =============================================================================

/// Model API an agent's harness should talk to. Secrets are referenced by the *name*
/// of an environment variable; the key itself is never stored here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiConfig {
    /// Wire protocol, e.g. `openai` or `anthropic`. Defaults to `openai` when unset.
    #[serde(default, alias = "format", skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Provider label such as `openrouter` or `local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Base URL of the endpoint. Configuration, not a secret.
    #[serde(
        default,
        alias = "url",
        alias = "endpoint",
        skip_serializing_if = "Option::is_none"
    )]
    pub base_url: Option<String>,
    /// Name of the environment variable holding the API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl ApiConfig {
    pub const DEFAULT_PROTOCOL: &'static str = "openai";

    pub fn protocol_or_default(&self) -> &str {
        self.protocol.as_deref().unwrap_or(Self::DEFAULT_PROTOCOL)
    }
}

/// A reusable agent definition: identity + harness + model + skills + launch options.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AgentProfile {
    /// Stable identifier used from the CLI and the TUI. Defaults to `name`, then to
    /// the harness name.
    pub id: String,
    /// Human-readable name; falls back to `id` for display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub harness: Harness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Skill names to install; `"*"` means every skill found.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Default first prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<String>,
    /// Extra CLI arguments appended after the generated ones.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Base58-encoded 32-byte Ed25519 public key (an `axel-identity` `PublicKey`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pubkey: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiConfig>,
    /// Working directory override, relative to the workspace or absolute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl AgentProfile {
    pub fn new(id: impl Into<String>, harness: Harness) -> Self {
        AgentProfile {
            id: id.into(),
            harness,
            ..Default::default()
        }
    }

    /// `name` when set, otherwise `id`.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// `true` when the profile asks for every available skill.
    pub fn wants_all_skills(&self) -> bool {
        self.skills.iter().any(|s| s == "*")
    }
}

#[derive(Deserialize)]
struct AgentProfileRaw {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    harness: Option<Harness>,
    /// Agent-level `command:` is accepted as a fallback for a custom harness.
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    skills: Vec<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    allowed_tools: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    disallowed_tools: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    args: Vec<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    pubkey: Option<String>,
    #[serde(default)]
    api: Option<ApiConfig>,
    #[serde(default)]
    path: Option<String>,
}

impl<'de> Deserialize<'de> for AgentProfile {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = AgentProfileRaw::deserialize(deserializer)?;
        let label = raw
            .id
            .clone()
            .or_else(|| raw.name.clone())
            .unwrap_or_else(|| "<unnamed>".to_string());

        let mut harness = raw.harness.ok_or_else(|| {
            de::Error::custom(format!("agent `{label}` requires a `harness` field"))
        })?;
        if harness.command.is_none() && raw.command.is_some() {
            harness.command = raw.command;
        }

        let id = raw
            .id
            .or(raw.name.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| match harness.kind.program() {
                Some(program) => program.to_string(),
                None => harness
                    .command
                    .as_deref()
                    .and_then(|c| c.split_whitespace().next())
                    .map(|c| {
                        Path::new(c)
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .unwrap_or_else(|| c.to_string())
                    })
                    .unwrap_or_else(|| "custom".to_string()),
            });

        if let Some(pubkey) = raw.pubkey.as_deref()
            && !is_base58_pubkey(pubkey)
        {
            return Err(de::Error::custom(format!(
                "agent `{id}`: pubkey must be a base58-encoded 32-byte Ed25519 public key"
            )));
        }

        Ok(AgentProfile {
            id,
            name: raw.name,
            harness,
            model: raw.model,
            skills: raw.skills,
            prompt: raw.prompt,
            allowed_tools: raw.allowed_tools,
            disallowed_tools: raw.disallowed_tools,
            args: raw.args,
            color: raw.color,
            pubkey: raw.pubkey,
            api: raw.api,
            path: raw.path,
        })
    }
}

/// Accept `key: value` as well as `key: [value, ...]`.
fn one_or_many<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(OneOrMany::One(s)) => vec![s],
        Some(OneOrMany::Many(v)) => v,
    })
}

/// Validate the canonical base58 representation of a 32-byte key (what Solana calls an
/// address). Ported from Axel v1.
pub fn is_base58_pubkey(value: &str) -> bool {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    if value.is_empty() || value.len() > 44 {
        return false;
    }

    let mut bytes = vec![0_u8];
    for character in value.bytes() {
        let Some(digit) = ALPHABET
            .iter()
            .position(|candidate| *candidate == character)
        else {
            return false;
        };

        let mut carry = digit as u32;
        for byte in bytes.iter_mut().rev() {
            carry += (*byte as u32) * 58;
            *byte = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.insert(0, (carry & 0xff) as u8);
            carry >>= 8;
        }
    }

    let leading_zeroes = value.bytes().take_while(|byte| *byte == b'1').count();
    let significant = bytes.iter().skip_while(|byte| **byte == 0).count();
    leading_zeroes + significant == 32
}

// =============================================================================
// Skill sources + workspace config
// =============================================================================

/// Where skills come from. Earlier entries win over later ones for duplicate names, and
/// directories on disk win over remote skills.
///
/// Written in `skills:` as a bare string or `- path: ...`:
///
/// - `./skills`, `~/skills`, `/abs/skills`: a directory of skills ([`SkillSource::Dir`]).
/// - `https://skills.sh/owner/repo/skill`, or the short form `owner/repo/skill`: one
///   skill from a GitHub repository, as listed on skills.sh.
/// - `github:owner/repo`, `github:owner/repo/skill`, either with `@ref`; or
///   `https://github.com/owner/repo[/tree/ref]`: a whole repo's skills, or one.
///
/// The short form is a path when that path exists next to `AXEL.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillSource {
    Dir(PathBuf),
    GitHub(GitHubSkills),
}

/// Skills in a GitHub repository, fetched with `git` and pinned in `AXEL.lock`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GitHubSkills {
    pub owner: String,
    pub repo: String,
    /// One skill by name; every skill in the repo when `None`.
    pub skill: Option<String>,
    /// Branch, tag or commit to follow; the default branch when `None`.
    pub git_ref: Option<String>,
}

impl GitHubSkills {
    /// The canonical spelling, used as the key in `AXEL.lock`: `owner/repo[/skill][@ref]`.
    pub fn key(&self) -> String {
        let mut k = format!("{}/{}", self.owner, self.repo);
        if let Some(skill) = &self.skill {
            k.push('/');
            k.push_str(skill);
        }
        if let Some(r) = &self.git_ref {
            k.push('@');
            k.push_str(r);
        }
        k
    }
}

impl fmt::Display for GitHubSkills {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

/// `name` is a plausible GitHub owner, repo or skill name.
fn is_slug(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `owner/repo[/skill]` with an optional `@ref` at the end.
fn github_path(path: &str, allow_whole_repo: bool) -> Option<GitHubSkills> {
    let (path, git_ref) = match path.rsplit_once('@') {
        Some((p, r)) if !r.is_empty() && !r.contains('/') => (p, Some(r.to_string())),
        Some(_) => return None,
        None => (path, None),
    };
    let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    let (owner, repo, skill) = match parts.as_slice() {
        [o, r] if allow_whole_repo => (*o, *r, None),
        [o, r, s] => (*o, *r, Some(s.to_string())),
        _ => return None,
    };
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (is_slug(owner) && is_slug(repo) && skill.as_deref().is_none_or(is_slug)).then(|| GitHubSkills {
        owner: owner.to_string(),
        repo: repo.to_string(),
        skill,
        git_ref,
    })
}

impl SkillSource {
    /// A directory source.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        SkillSource::Dir(path.into())
    }

    /// Read one `skills:` entry. `base` is the manifest's directory: the short form
    /// `owner/repo/skill` stays a path when it exists there.
    pub fn parse(text: &str, base: Option<&Path>) -> Result<SkillSource, Error> {
        let text = text.trim();
        let bad = || Error::Invalid(format!("unrecognised skill source `{text}`"));
        for prefix in ["https://skills.sh/", "http://skills.sh/", "skills.sh/"] {
            if let Some(rest) = text.strip_prefix(prefix) {
                return github_path(rest, true).map(SkillSource::GitHub).ok_or_else(bad);
            }
        }
        if let Some(rest) = text.strip_prefix("github:") {
            return github_path(rest, true).map(SkillSource::GitHub).ok_or_else(bad);
        }
        for prefix in ["https://github.com/", "http://github.com/", "github.com/"] {
            if let Some(rest) = text.strip_prefix(prefix) {
                // owner/repo, owner/repo/tree/<ref>[/...]: the ref, never a sub-path.
                let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
                let (owner, repo) = match parts.as_slice() {
                    [o, r, ..] => (*o, *r),
                    _ => return Err(bad()),
                };
                let git_ref = match parts.get(2..) {
                    Some([kind, r, ..]) if *kind == "tree" || *kind == "commit" => Some(r.to_string()),
                    Some([]) | None => None,
                    _ => return Err(bad()),
                };
                let mut gh = github_path(&format!("{owner}/{repo}"), true).ok_or_else(bad)?;
                gh.git_ref = git_ref;
                return Ok(SkillSource::GitHub(gh));
            }
        }
        if text.contains("://") {
            return Err(bad());
        }
        let looks_local = text.starts_with(['.', '/', '~']);
        if !looks_local
            && let Some(gh) = github_path(text, false)
        {
            let exists = base.map(|b| b.join(text)).unwrap_or_else(|| PathBuf::from(text)).exists();
            if !exists {
                return Ok(SkillSource::GitHub(gh));
            }
        }
        Ok(SkillSource::Dir(PathBuf::from(text)))
    }

    /// A directory source's path with a leading `~` expanded and a relative path joined
    /// to `base`. `None` for remote sources.
    pub fn resolve(&self, base: Option<&Path>) -> Option<PathBuf> {
        let SkillSource::Dir(path) = self else { return None };
        let expanded = expand_tilde(path);
        Some(match base {
            Some(base) if expanded.is_relative() => base.join(expanded),
            _ => expanded,
        })
    }
}

impl fmt::Display for SkillSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillSource::Dir(p) => write!(f, "{}", p.display()),
            SkillSource::GitHub(g) => write!(f, "github:{g}"),
        }
    }
}

/// A `skills:` entry as written, before it is classified.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawSkillSource {
    Text(String),
    Path { path: String },
}

/// Expand a leading `~` or `~/` to the home directory. Other paths are returned as-is.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    if text == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    match (text.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

/// The parsed `AXEL.md`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceConfig {
    /// `workspace:` (alias `name:`) from the frontmatter.
    pub workspace: Option<String>,
    /// Skill sources in priority order. After [`WorkspaceConfig::load`] relative
    /// directories are already resolved against the manifest's directory.
    pub skills: Vec<SkillSource>,
    /// Markdown after the frontmatter (the project context), trimmed.
    pub body: String,
    /// Where the manifest was read from, when it was read from disk.
    pub path: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
struct Frontmatter {
    #[serde(default, alias = "name")]
    workspace: Option<String>,
    #[serde(default)]
    skills: Vec<RawSkillSource>,
}

impl WorkspaceConfig {
    /// Read `dir/AXEL.md`. `Ok(None)` when the file does not exist.
    pub fn load(dir: &Path) -> Result<Option<WorkspaceConfig>, Error> {
        let path = dir.join(AXEL_MD);
        if !path.is_file() {
            return Ok(None);
        }
        Self::load_file(&path).map(Some)
    }

    /// Read a manifest at an explicit path, resolving relative skill sources against
    /// its directory.
    pub fn load_file(path: &Path) -> Result<WorkspaceConfig, Error> {
        let text = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let base = path.parent().filter(|p| !p.as_os_str().is_empty());
        let mut config = Self::parse_in(&text, base).map_err(|e| e.at(path))?;
        config.path = Some(path.to_path_buf());
        for source in &mut config.skills {
            if let SkillSource::Dir(dir) = source
                && dir.is_relative()
                && !dir.starts_with("~")
            {
                *dir = base.map(|b| b.join(&*dir)).unwrap_or_else(|| dir.clone());
            }
        }
        Ok(config)
    }

    /// Parse manifest text. Frontmatter is optional: text without a leading `---` line
    /// is all body, which makes a plain markdown `AXEL.md` a valid manifest.
    pub fn parse(text: &str) -> Result<WorkspaceConfig, Error> {
        Self::parse_in(text, None)
    }

    fn parse_in(text: &str, base: Option<&Path>) -> Result<WorkspaceConfig, Error> {
        let (yaml, body) = split_frontmatter(text)?;
        let frontmatter: Frontmatter = match yaml {
            Some(yaml) if !yaml.trim().is_empty() => serde_norway::from_str(yaml)?,
            _ => Frontmatter::default(),
        };
        let skills = frontmatter
            .skills
            .into_iter()
            .map(|raw| match raw {
                RawSkillSource::Text(t) => SkillSource::parse(&t, base),
                RawSkillSource::Path { path } => Ok(SkillSource::Dir(PathBuf::from(path))),
            })
            .collect::<Result<_, _>>()?;
        Ok(WorkspaceConfig {
            workspace: frontmatter.workspace.filter(|w| !w.trim().is_empty()),
            skills,
            body: body.trim().to_string(),
            path: None,
        })
    }

    /// Directory containing the manifest, when known.
    pub fn dir(&self) -> Option<&Path> {
        self.path.as_deref().and_then(Path::parent)
    }

    /// Workspace name: `workspace:` if set, otherwise the manifest's directory name.
    pub fn name(&self) -> Option<String> {
        self.workspace.clone().or_else(|| {
            self.dir()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
        })
    }

    /// Skill directories that exist on disk, resolved and in priority order.
    pub fn skills_dirs(&self) -> Vec<PathBuf> {
        self.skills
            .iter()
            .filter_map(|s| s.resolve(self.dir()))
            .filter(|p| p.is_dir())
            .collect()
    }

    /// The remote sources, in order.
    pub fn remote_skills(&self) -> Vec<&GitHubSkills> {
        self.skills
            .iter()
            .filter_map(|s| match s {
                SkillSource::GitHub(g) => Some(g),
                SkillSource::Dir(_) => None,
            })
            .collect()
    }

    /// A fresh `AXEL.md` for a workspace. Round-trips through [`WorkspaceConfig::parse`].
    pub fn template(workspace_name: &str) -> String {
        format!(
            r#"---
workspace: {workspace}

# Where skills come from (first match wins for duplicate names; directories on disk
# win over remote skills). A skill is `<name>.md` or `<name>/SKILL.md`.
#   ./skills                              a directory next to this file
#   https://skills.sh/owner/repo/skill    one skill listed on skills.sh
#   owner/repo/skill                      the same, short
#   github:owner/repo@v1                  every skill in a repo, at a tag
# Remote skills are pinned to a commit in AXEL.lock; `axel skills update` moves them.
skills:
  - ./skills
---

# {workspace}

<!-- Project context for agents. Axel links this file as CLAUDE.md / AGENTS.md. -->

## Overview

<!-- Brief description of what this project does -->

## Getting Started

<!-- How to set up and run the project -->

## Architecture

<!-- High-level architecture overview -->

## Key Files

<!-- Important files and directories -->
"#,
            workspace = workspace_name,
        )
    }
}

/// Split text into `(frontmatter yaml, body)`.
///
/// Frontmatter is present when the first non-blank line is exactly `---`; it ends at
/// the next `---` line. Without an opening `---` the whole text is the body. An opening
/// `---` without a closing one is [`Error::UnterminatedFrontmatter`].
pub fn split_frontmatter(text: &str) -> Result<(Option<&str>, &str), Error> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text).trim_start();
    let mut offset = 0;
    let mut yaml_start = None;

    while offset < text.len() {
        let rest = &text[offset..];
        let line_len = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
        let line = rest[..line_len].trim_end();
        let next = offset + line_len;

        match yaml_start {
            None => {
                if line != "---" {
                    return Ok((None, text));
                }
                yaml_start = Some(next);
            }
            Some(start) if line == "---" => {
                return Ok((Some(&text[start..offset]), &text[next..]));
            }
            Some(_) => {}
        }
        offset = next;
    }

    match yaml_start {
        None => Ok((None, text)),
        Some(_) => Err(Error::UnterminatedFrontmatter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_profiles_still_deserialize() {
        let agent: AgentProfile = serde_norway::from_str(
            "id: reviewer\nname: Atlas\nharness: claude\nmodel: opus\ndisallowed_tools: Bash\n",
        )
        .unwrap();
        assert_eq!(agent.display_name(), "Atlas");
        assert_eq!(agent.harness, Harness::builtin(HarnessKind::Claude));
        assert_eq!(agent.disallowed_tools, ["Bash"]);
        let err = serde_norway::from_str::<AgentProfile>("id: x\nharness: claude\npubkey: nope\n").unwrap_err();
        assert!(err.to_string().contains("base58"), "{err}");
    }

    fn gh(owner: &str, repo: &str, skill: Option<&str>, git_ref: Option<&str>) -> SkillSource {
        SkillSource::GitHub(GitHubSkills {
            owner: owner.into(),
            repo: repo.into(),
            skill: skill.map(Into::into),
            git_ref: git_ref.map(Into::into),
        })
    }

    #[test]
    fn skill_sources_parse() {
        let p = |t: &str| SkillSource::parse(t, None).unwrap();
        assert_eq!(p("https://skills.sh/mattpocock/skills/grill-me"), gh("mattpocock", "skills", Some("grill-me"), None));
        assert_eq!(p("skills.sh/vercel-labs/skills"), gh("vercel-labs", "skills", None, None));
        assert_eq!(p("mattpocock/skills/grill-me"), gh("mattpocock", "skills", Some("grill-me"), None));
        assert_eq!(p("mattpocock/skills/grill-me@v2"), gh("mattpocock", "skills", Some("grill-me"), Some("v2")));
        assert_eq!(p("github:acme/agent-skills@v2"), gh("acme", "agent-skills", None, Some("v2")));
        assert_eq!(p("github:acme/agent-skills/review"), gh("acme", "agent-skills", Some("review"), None));
        assert_eq!(p("https://github.com/acme/agent-skills.git"), gh("acme", "agent-skills", None, None));
        assert_eq!(p("https://github.com/acme/agent-skills/tree/main"), gh("acme", "agent-skills", None, Some("main")));
        assert_eq!(p("./skills"), SkillSource::new("./skills"));
        assert_eq!(p("~/.config/axel/skills"), SkillSource::new("~/.config/axel/skills"));
        assert_eq!(p("skills"), SkillSource::new("skills"), "one segment is a path");
        assert_eq!(p("my/skills"), SkillSource::new("my/skills"), "two segments are a path");
        assert!(SkillSource::parse("https://example.com/x", None).is_err());
        assert!(SkillSource::parse("github:just-owner", None).is_err());
        assert_eq!(gh("a", "b", Some("c"), Some("v1")).to_string(), "github:a/b/c@v1");

        // The short form is a path when it exists next to the manifest.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("team/shared/skills")).unwrap();
        assert_eq!(
            SkillSource::parse("team/shared/skills", Some(dir.path())).unwrap(),
            SkillSource::new("team/shared/skills")
        );
    }

    #[test]
    fn missing_frontmatter_is_all_body() {
        let config = WorkspaceConfig::parse("# Just notes\n\nNo config here.\n").unwrap();
        assert!(config.workspace.is_none());
        assert!(config.skills.is_empty());
        assert_eq!(config.body, "# Just notes\n\nNo config here.");

        let config = WorkspaceConfig::parse("").unwrap();
        assert_eq!(config, WorkspaceConfig::default());
    }

    #[test]
    fn unterminated_frontmatter_is_an_error() {
        let err = WorkspaceConfig::parse("---\nworkspace: x\n# oops").unwrap_err();
        assert!(matches!(err, Error::UnterminatedFrontmatter));
    }

    #[test]
    fn empty_frontmatter_and_legacy_keys_are_tolerated() {
        let config = WorkspaceConfig::parse("---\n---\nbody").unwrap();
        assert_eq!(config.body, "body");

        let config = WorkspaceConfig::parse(
            "---\nworkspace: old\nlayouts:\n  grids:\n    default:\n      type: tmux\nagents:\n  - id: a\n    harness: claude\n---\n",
        )
        .unwrap();
        assert_eq!(config.workspace.as_deref(), Some("old"), "agents: and layouts: are ignored");
    }

    #[test]
    fn template_roundtrips() {
        let text = WorkspaceConfig::template("myproj");
        let config = WorkspaceConfig::parse(&text).unwrap();
        assert_eq!(config.workspace.as_deref(), Some("myproj"));
        assert_eq!(config.skills, [SkillSource::new("./skills")]);
        assert!(config.body.starts_with("# myproj"));
        assert!(config.body.contains("## Key Files"));
    }

    #[test]
    fn load_resolves_relative_skill_sources() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(AXEL_MD),
            "---\nskills:\n  - path: ./skills\n  - ~/.config/axel/skills\n  - /abs\n---\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("skills")).unwrap();

        let config = WorkspaceConfig::load(dir.path()).unwrap().unwrap();
        assert_eq!(
            config.path.as_deref(),
            Some(dir.path().join(AXEL_MD).as_path())
        );
        assert_eq!(config.skills[0], SkillSource::new(dir.path().join("skills")));
        assert_eq!(config.skills[1], SkillSource::new("~/.config/axel/skills"));
        assert_eq!(config.skills[2], SkillSource::new("/abs"));
        assert_eq!(config.skills_dirs(), vec![dir.path().join("skills")]);
        assert_eq!(
            config.name().as_deref(),
            dir.path().file_name().and_then(|n| n.to_str())
        );

        assert!(
            WorkspaceConfig::load(&dir.path().join("nope"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn harness_serializes_compactly() {
        let yaml = serde_norway::to_string(&Harness::builtin(HarnessKind::Codex)).unwrap();
        assert_eq!(yaml.trim(), "codex");
        let yaml =
            serde_norway::to_string(&Harness::custom("npm run dev", vec!["--x".into()])).unwrap();
        let back: Harness = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(back, Harness::custom("npm run dev", vec!["--x".into()]));
    }

    #[test]
    fn split_frontmatter_edge_cases() {
        assert_eq!(
            split_frontmatter("---\na: 1\n---").unwrap(),
            (Some("a: 1\n"), "")
        );
        assert_eq!(
            split_frontmatter("---\r\na: 1\r\n---\r\nbody").unwrap(),
            (Some("a: 1\r\n"), "body")
        );
        assert_eq!(
            split_frontmatter("\n\n---\n---\nx").unwrap(),
            (Some(""), "x")
        );
        assert_eq!(
            split_frontmatter("--- not\nx").unwrap(),
            (None, "--- not\nx")
        );
    }

    #[test]
    fn base58_pubkey_validation() {
        assert!(is_base58_pubkey("11111111111111111111111111111111"));
        assert!(is_base58_pubkey(
            "So11111111111111111111111111111111111111112"
        ));
        assert!(!is_base58_pubkey("not-a-key"));
        assert!(!is_base58_pubkey(""));
    }
}
