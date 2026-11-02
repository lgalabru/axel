//! `get_agent` — full details for a single agent.

use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::context::CallScope;

/// Input parameters for the `get_agent` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    /// Agent name or base58 public key.
    #[schemars(
        description = "Agent name (e.g. \"atlas\") or base58-encoded Ed25519 public key."
    )]
    pub agent: String,
}

/// Full agent details returned to the MCP client.
#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentDetails {
    /// Agent display name.
    #[schemars(description = "Agent display name")]
    pub name: String,
    /// Base58-encoded Ed25519 public key.
    #[schemars(description = "Base58-encoded Ed25519 public key")]
    pub pubkey: String,
    /// Harness this agent uses.
    #[schemars(description = "Harness")]
    pub harness: String,
    /// Model override, if configured.
    #[schemars(description = "Model override")]
    pub model: Option<String>,
    /// Enabled skill names.
    #[schemars(description = "Enabled skill names")]
    pub skills: Vec<String>,
    /// System prompt.
    #[schemars(description = "System prompt")]
    pub prompt: Option<String>,
    /// TUI colour.
    #[schemars(description = "TUI colour")]
    pub color: Option<String>,
    /// Runtime mode.
    #[schemars(description = "Runtime mode")]
    pub runtime_mode: String,
    /// Whether the signing key is present in this machine's keystore.
    #[schemars(description = "True if the agent's signing key is in this machine's keystore")]
    pub key_in_keystore: bool,
    /// Whether the owner attestation is valid.
    #[schemars(description = "Owner attestation validity: \"valid\", \"invalid\", or \"none\"")]
    pub attestation: String,
    /// Owner public key from the attestation, if present.
    #[schemars(description = "Owner public key from the attestation tag")]
    pub attestation_owner: Option<String>,
    /// Whether the agent is archived.
    #[schemars(description = "True if the agent is archived")]
    pub archived: bool,
    /// Unix timestamp when the agent was created.
    #[schemars(description = "Unix timestamp of agent creation")]
    pub created_at: i64,
}

pub fn run(params: Params, scope: &CallScope) -> Result<CallToolResult, rmcp::ErrorData> {
    let query = params.agent.trim();

    // Try by name first, then by pubkey.
    let agent = scope
        .store
        .agent_by_name(query)
        .map_err(|e| rmcp::ErrorData::internal_error(format!("store error: {e}"), None))?
        .or_else(|| {
            scope
                .store
                .agent_by_pubkey(query)
                .ok()
                .flatten()
        });

    let Some(agent) = agent else {
        return Ok(super::tool_error(format!(
            "No agent found matching \"{query}\"."
        )));
    };

    // Check keystore.
    let key_in_keystore = agent
        .pubkey
        .parse()
        .ok()
        .and_then(|pk| scope.keystore.agent(&pk).ok().flatten())
        .is_some();

    // Check attestation.
    let (attestation, attestation_owner) = match &agent.auth_tag {
        Some(tag) => {
            match axel_identity::AuthTag::from_tag(tag) {
                Ok(auth_tag) => {
                    let owner_str = auth_tag.owner.to_string();
                    let valid = agent
                        .pubkey
                        .parse()
                        .ok()
                        .map(|pk| axel_identity::verify_attestation(&auth_tag, &pk).is_ok())
                        .unwrap_or(false);
                    (
                        if valid { "valid" } else { "invalid" }.to_string(),
                        Some(owner_str),
                    )
                }
                Err(_) => ("invalid".to_string(), None),
            }
        }
        None => ("none".to_string(), None),
    };

    let details = AgentDetails {
        name: agent.name,
        pubkey: agent.pubkey,
        harness: agent.harness.to_string(),
        model: agent.model,
        skills: agent.skills,
        prompt: agent.prompt,
        color: agent.color.map(|c| c.to_string()),
        runtime_mode: agent.runtime_mode.to_string(),
        key_in_keystore,
        attestation,
        attestation_owner,
        archived: agent.archived_at.is_some(),
        created_at: agent.created_at,
    };

    let json =
        serde_json::to_string_pretty(&details).unwrap_or_else(|_| "{}".to_string());

    Ok(CallToolResult::success(vec![
        rmcp::model::ContentBlock::text(json),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_deserializes() {
        let json = r#"{"agent": "atlas"}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert_eq!(params.agent, "atlas");
    }
}
