//! MCP tool modules — one per tool, each with a `Params` struct and a `run()` function.

pub mod create_agent;
pub mod get_agent;
pub mod list_agents;
pub mod read_inbox;
pub mod send;
pub mod set_status;

/// Build a `CallToolResult` that carries an error flag the MCP client can
/// inspect, while still rendering the message as visible text.
pub(crate) fn tool_error(message: impl Into<String>) -> rmcp::model::CallToolResult {
    rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text(message.into())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_error_returns_visible_error_content() {
        let result = tool_error("visible failure");

        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.content[0].as_text().unwrap().text, "visible failure");
    }
}
