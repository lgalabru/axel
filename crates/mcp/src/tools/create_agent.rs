//! `create_agent` — generate a key, attest it, store it.
//!
//! Replicates the same sequence as `axel agent new` (see
//! `crates/axel/src/cmd/agent.rs::create()`), calling the shared core in
//! `axel-identity` and `axel-store` directly.

use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::context::CallScope;

/// Input parameters for the `create_agent` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    /// Agent name: must match `[A-Za-z0-9_-]+` so it works as an @mention handle.
    #[schemars(description = "Agent name. Must be alphanumeric with hyphens/underscores \
        (e.g. \"atlas\", \"code-reviewer\"). Used as @mention handle.")]
    pub name: String,

    /// Harness (coding CLI) this agent runs in: "claude", "codex", "opencode",
    /// or any custom command.
    #[schemars(
        description = "Harness (coding CLI) to use: \"claude\", \"codex\", \"opencode\", \
            or a custom command string. Defaults to \"claude\"."
    )]
    #[serde(default = "default_harness")]
    pub harness: String,

    /// Model override passed to the harness (e.g. "opus", "o3", "gpt-4o").
    #[schemars(
        description = "Model override for the harness (e.g. \"opus\", \"o3\"). \
            Omit to use the harness default."
    )]
    #[serde(default)]
    pub model: Option<String>,

    /// Comma-separated skill names, or `"*"` to enable all discovered skills.
    #[schemars(
        description = "Comma-separated skill names to enable, or \"*\" for all. Defaults to \"*\"."
    )]
    #[serde(default = "default_skills")]
    pub skills: String,

    /// System prompt injected into the harness session.
    #[schemars(
        description = "System prompt injected into every session of this agent. Optional."
    )]
    #[serde(default)]
    pub prompt: Option<String>,

    /// Agent colour in the TUI dashboard.
    #[schemars(
        description = "Agent colour in the TUI: \"purple\", \"yellow\", \"red\", \
            \"green\", \"blue\", \"gray\", or \"orange\". Omit for automatic assignment."
    )]
    #[serde(default)]
    pub color: Option<String>,

    /// How much the agent may do unattended.
    #[schemars(
        description = "Runtime mode: \"autopilot\" (never asks, default), \"edits\" \
            (auto-accepts file edits), \"supervised\" (asks for everything), \
            \"plan\" (read-only)."
    )]
    #[serde(default = "default_runtime_mode")]
    pub runtime_mode: String,
}

fn default_harness() -> String {
    "claude".to_string()
}

fn default_skills() -> String {
    "*".to_string()
}

fn default_runtime_mode() -> String {
    "autopilot".to_string()
}

/// Successful result returned to the MCP client.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CreateAgentResult {
    /// The agent's display name.
    #[schemars(description = "Agent display name")]
    pub name: String,
    /// Base58-encoded Ed25519 public key (the agent's identity).
    #[schemars(description = "Base58-encoded Ed25519 public key")]
    pub pubkey: String,
    /// Harness the agent is configured to use.
    #[schemars(description = "Configured harness")]
    pub harness: String,
    /// Model override, if set.
    #[schemars(description = "Model override, if configured")]
    pub model: Option<String>,
    /// Runtime mode.
    #[schemars(description = "Runtime mode")]
    pub runtime_mode: String,
}

pub fn run(params: Params, scope: &CallScope) -> Result<CallToolResult, rmcp::ErrorData> {
    let name = params.name.trim();
    if !is_mention_handle(name) {
        return Ok(super::tool_error(
            "Agent names must match [A-Za-z0-9_-]+ so they work as @mentions.",
        ));
    }

    let existing = scope
        .store
        .agent_by_name(name)
        .map_err(|e| rmcp::ErrorData::internal_error(format!("store error: {e}"), None))?;
    if existing.is_some() {
        return Ok(super::tool_error(format!(
            "An agent named \"{name}\" already exists."
        )));
    }

    let name = name.to_string();
    let new_agent = new_agent(&name, params);
    let agent = insert_keyed_agent(scope, new_agent)?;

    let result = CreateAgentResult {
        name: agent.name,
        pubkey: agent.pubkey,
        harness: agent.harness.to_string(),
        model: agent.model,
        runtime_mode: agent.runtime_mode.to_string(),
    };
    super::json_success(&result)
}

/// Whether `name` works as an @mention handle: non-empty, letters, digits,
/// `-` and `_` only.
fn is_mention_handle(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// The agent record `params` describe, still without its key and tag.
fn new_agent(name: &str, params: Params) -> axel_store::NewAgent {
    use axel_store::{AgentColor, Harness, NewAgent, RuntimeMode};

    // An unknown colour leaves it unset, which the TUI reads as "assign one
    // automatically", as the parameter's description promises.
    let color: Option<AgentColor> = params.color.as_deref().and_then(|s| s.parse().ok());
    // An unknown mode falls back to the default (autopilot), the same as
    // leaving the parameter out.
    let runtime_mode: RuntimeMode = params.runtime_mode.parse().unwrap_or_default();
    let skills: Vec<String> = params
        .skills
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect();
    NewAgent {
        pubkey: String::new(),
        name: name.to_string(),
        harness: Harness::parse(&params.harness),
        model: params.model,
        effort: None,
        skills,
        prompt: params.prompt,
        color,
        runtime_mode,
        auth_tag: None,
        profile_id: None,
        device: None,
    }
}

/// Generate a key for `agent`, store it, attest it with the owner key and
/// insert the agent record.
fn insert_keyed_agent(scope: &CallScope, mut agent: axel_store::NewAgent) -> Result<axel_store::Agent, rmcp::ErrorData> {
    use axel_identity::{Keypair, attest};

    let keypair = Keypair::generate();
    scope
        .keystore
        .put_agent(&keypair)
        .map_err(|e| rmcp::ErrorData::internal_error(format!("keystore error: {e}"), None))?;

    let owner = scope
        .keystore
        .owner()
        .map_err(|e| rmcp::ErrorData::internal_error(format!("owner key error: {e}"), None))?;
    let tag = attest(&owner, &keypair.public(), "")
        .map_err(|e| rmcp::ErrorData::internal_error(format!("attestation error: {e}"), None))?;

    agent.pubkey = keypair.public().to_string();
    agent.auth_tag = Some(tag.to_tag());
    scope
        .store
        .insert_agent(&agent)
        .map_err(|e| rmcp::ErrorData::internal_error(format!("insert error: {e}"), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_harness_is_claude() {
        assert_eq!(default_harness(), "claude");
    }

    #[test]
    fn default_skills_is_wildcard() {
        assert_eq!(default_skills(), "*");
    }

    #[test]
    fn default_runtime_mode_is_autopilot() {
        assert_eq!(default_runtime_mode(), "autopilot");
    }

    #[test]
    fn params_deserializes_with_defaults() {
        let json = r#"{"name": "atlas"}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "atlas");
        assert_eq!(params.harness, "claude");
        assert_eq!(params.skills, "*");
        assert_eq!(params.runtime_mode, "autopilot");
        assert!(params.model.is_none());
        assert!(params.prompt.is_none());
        assert!(params.color.is_none());
    }

    #[test]
    fn params_deserializes_full() {
        let json = r#"{
            "name": "test-bot",
            "harness": "codex",
            "model": "o3",
            "skills": "review,test",
            "prompt": "You review code.",
            "color": "green",
            "runtime_mode": "supervised"
        }"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "test-bot");
        assert_eq!(params.harness, "codex");
        assert_eq!(params.model.as_deref(), Some("o3"));
        assert_eq!(params.skills, "review,test");
        assert_eq!(params.prompt.as_deref(), Some("You review code."));
        assert_eq!(params.color.as_deref(), Some("green"));
        assert_eq!(params.runtime_mode, "supervised");
    }
}
