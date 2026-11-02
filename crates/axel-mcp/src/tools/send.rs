//! `send` — message other agents on this thread, as `axel send` does.

use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::context::AxelContext;

/// Input parameters for the `send` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    /// Agent names to send to, without the `@`.
    #[schemars(description = "Names of the agents to send to, without '@' (e.g. [\"atlas\"]); \"atlas@mini\" runs atlas on host mini if it has to be summoned. See list_agents.")]
    pub to: Vec<String>,
    /// The message. Self-contained: the recipient can't see your conversation.
    #[schemars(
        description = "The message. Make it self-contained: the recipient can't see your conversation, so include paths, \
            what you tried and what you want back. Say if you need a reply."
    )]
    pub text: String,
}

pub fn run(params: Params, ctx: &dyn AxelContext) -> Result<CallToolResult, rmcp::ErrorData> {
    let to: Vec<String> = params.to.iter().map(|n| n.trim().trim_start_matches('@').to_string()).filter(|n| !n.is_empty()).collect();
    if to.is_empty() {
        return Ok(super::tool_error("Name at least one agent in `to`."));
    }
    if params.text.trim().is_empty() {
        return Ok(super::tool_error("The message is empty."));
    }
    match ctx.send(&to, &params.text) {
        Ok(report) => {
            let json = serde_json::to_string_pretty(&report).unwrap_or_default();
            let failed = report.deliveries.iter().any(|d| matches!(d.outcome, crate::context::Outcome::Failed { .. }));
            let content = vec![rmcp::model::ContentBlock::text(json)];
            Ok(if failed { CallToolResult::error(content) } else { CallToolResult::success(content) })
        }
        Err(e) => Ok(super::tool_error(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{CallScope, Delivery, Outcome, SendReport};
    use std::sync::Mutex;

    struct Fake(Mutex<Vec<(Vec<String>, String)>>);
    impl AxelContext for Fake {
        fn scope(&self) -> Result<CallScope, rmcp::ErrorData> {
            Err(rmcp::ErrorData::internal_error("no store", None))
        }
        fn send(&self, to: &[String], text: &str) -> Result<SendReport, String> {
            self.0.lock().unwrap().push((to.to_vec(), text.to_string()));
            if to.iter().any(|t| t == "nobody") {
                return Err("nova may not message nobody; allow it with `axel agent edit nova --allow nobody`".into());
            }
            Ok(SendReport {
                message_id: "abc".into(),
                thread: Some("t1".into()),
                deliveries: to.iter().map(|t| Delivery { to: t.clone(), outcome: Outcome::Queued }).collect(),
            })
        }
    }

    #[test]
    fn names_lose_their_at_and_outcomes_come_back_as_json() {
        let fake = Fake(Mutex::new(Vec::new()));
        let r = run(Params { to: vec!["@atlas".into(), " ".into()], text: "review src/auth.rs".into() }, &fake).unwrap();
        assert_eq!(fake.0.lock().unwrap()[0].0, vec!["atlas".to_string()]);
        assert_ne!(r.is_error, Some(true));
        let text = &r.content[0].as_text().unwrap().text;
        assert!(text.contains("\"outcome\": \"queued\"") && text.contains("\"to\": \"atlas\""), "{text}");
    }

    #[test]
    fn refusals_are_tool_errors_with_the_reason() {
        let fake = Fake(Mutex::new(Vec::new()));
        let r = run(Params { to: vec!["nobody".into()], text: "hi".into() }, &fake).unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(r.content[0].as_text().unwrap().text.contains("axel agent edit"));
        let r = run(Params { to: vec![], text: "hi".into() }, &fake).unwrap();
        assert_eq!(r.is_error, Some(true));
    }
}
