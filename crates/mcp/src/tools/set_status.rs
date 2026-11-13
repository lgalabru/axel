//! `set_status` — report this session's status, as `axel status` does.

use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::context::{AxelContext, BlockedKind, StatusState};

/// Input parameters for the `set_status` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    #[schemars(description = "working, blocked (only the owner can unblock you), done (the task is finished), idle or error")]
    pub state: StatusState,
    #[schemars(description = "One line the owner sees on the dashboard, e.g. \"migrating the auth tests\"")]
    #[serde(default)]
    pub message: Option<String>,
    #[schemars(description = "For blocked: what you wait for (permission, question or auth)")]
    #[serde(default)]
    pub kind: Option<BlockedKind>,
}

pub fn run(params: Params, ctx: &dyn AxelContext) -> Result<CallToolResult, rmcp::ErrorData> {
    match ctx.set_status(params.state, params.message.as_deref(), params.kind) {
        Ok(()) => Ok(CallToolResult::success(vec![rmcp::model::ContentBlock::text(format!("status: {}", params.state.as_str()))])),
        Err(e) => Ok(super::tool_error(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_parse_states_and_kinds() {
        let p: Params = serde_json::from_str(r#"{"state":"blocked","message":"which DB?","kind":"question"}"#).unwrap();
        assert_eq!(p.state, StatusState::Blocked);
        assert_eq!(p.kind, Some(BlockedKind::Question));
        assert!(serde_json::from_str::<Params>(r#"{"state":"sleeping"}"#).is_err());
    }
}
