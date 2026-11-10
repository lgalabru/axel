//! `list_agents` — return all registered agents.

use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::context::CallScope;

/// Input parameters for the `list_agents` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    /// When true, include agents that have been archived.
    #[schemars(description = "Include archived agents in the listing. Defaults to false.")]
    #[serde(default)]
    pub include_archived: bool,
}

/// One agent in the response list.
#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentEntry {
    /// Agent display name (the @mention handle).
    #[schemars(description = "Agent display name")]
    pub name: String,
    /// Base58-encoded Ed25519 public key.
    #[schemars(description = "Base58-encoded Ed25519 public key")]
    pub pubkey: String,
    /// Harness this agent uses.
    #[schemars(description = "Harness (coding CLI) this agent runs in")]
    pub harness: String,
    /// Model override, if configured.
    #[schemars(description = "Model override")]
    pub model: Option<String>,
    /// Reasoning effort, if configured: low, medium, high, xhigh, or max.
    #[schemars(description = "Reasoning effort; null means the harness's default")]
    pub effort: Option<String>,
    /// Runtime mode: autopilot, edits, supervised, or plan.
    #[schemars(description = "Runtime mode")]
    pub runtime_mode: String,
    /// Whether the agent has been archived.
    #[schemars(description = "True if the agent is archived")]
    pub archived: bool,
    /// Number of unread inbox messages.
    #[schemars(description = "Number of unread inbox messages for this agent")]
    pub unread: u64,
}

pub fn run(params: Params, scope: &CallScope) -> Result<CallToolResult, rmcp::ErrorData> {
    let agents = scope
        .store
        .list_agents()
        .map_err(|e| rmcp::ErrorData::internal_error(format!("store error: {e}"), None))?;

    let entries: Vec<AgentEntry> = agents
        .into_iter()
        .filter(|a| params.include_archived || a.archived_at.is_none())
        .map(|a| {
            let unread = scope.store.unread_count(&a.pubkey).unwrap_or(0);
            AgentEntry {
                name: a.name,
                pubkey: a.pubkey,
                harness: a.harness.to_string(),
                model: a.model,
                effort: a.effort.map(|e| e.to_string()),
                runtime_mode: a.runtime_mode.to_string(),
                archived: a.archived_at.is_some(),
                unread,
            }
        })
        .collect();

    let json =
        serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string());

    Ok(CallToolResult::success(vec![
        rmcp::model::ContentBlock::text(json),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_defaults_to_no_archived() {
        let json = r#"{}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert!(!params.include_archived);
    }

    #[test]
    fn params_accepts_include_archived() {
        let json = r#"{"include_archived": true}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert!(params.include_archived);
    }
}
