//! `read_inbox` — messages addressed to the caller.

use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::context::AxelContext;

/// Input parameters for the `read_inbox` tool.
/// Messages with `handled: true` already reached the caller as prompts or
/// notes on their thread: they're history, not requests.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    #[schemars(description = "How many of the newest messages to return (default 20, at most 100)")]
    #[serde(default)]
    pub limit: Option<usize>,
}

pub fn run(params: Params, ctx: &dyn AxelContext) -> Result<CallToolResult, rmcp::ErrorData> {
    match ctx.inbox(params.limit.unwrap_or(20).clamp(1, 100)) {
        Ok(msgs) => Ok(CallToolResult::success(vec![rmcp::model::ContentBlock::text(
            serde_json::to_string_pretty(&msgs).unwrap_or_else(|_| "[]".into()),
        )])),
        Err(e) => Ok(super::tool_error(e)),
    }
}
