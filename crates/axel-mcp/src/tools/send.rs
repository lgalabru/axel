//! `send` — message other agents on this thread, as `axel send` does.

use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

#[cfg(test)]
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
    /// No reply needed and nothing to act on.
    #[schemars(
        description = "true for news that needs no reply and nothing done (\"FYI: the tests pass now\"). It doesn't start the \
            recipient's turn: it reads it with its next one. It never summons an agent that isn't on the thread. \
            Leave it out for a request or a question."
    )]
    #[serde(default)]
    pub fyi: bool,
}

/// The recipients (names without `@`) and the kind, or what's wrong with the call.
pub fn validate(params: &Params) -> Result<(Vec<String>, crate::context::MessageKind), &'static str> {
    let to: Vec<String> = params.to.iter().map(|n| n.trim().trim_start_matches('@').to_string()).filter(|n| !n.is_empty()).collect();
    if to.is_empty() {
        return Err("Name at least one agent in `to`.");
    }
    if params.text.trim().is_empty() {
        return Err("The message is empty.");
    }
    let kind = if params.fyi { crate::context::MessageKind::Fyi } else { crate::context::MessageKind::Request };
    Ok((to, kind))
}

/// The tool result for what `send` did.
pub fn finish(result: Result<crate::context::SendReport, crate::context::SendError>) -> CallToolResult {
    match result {
        Ok(report) => {
            let json = serde_json::to_string_pretty(&report).unwrap_or_default();
            let failed = report.deliveries.iter().any(|d| matches!(d.outcome, crate::context::Outcome::Failed { .. }));
            let content = vec![rmcp::model::ContentBlock::text(json)];
            if failed { CallToolResult::error(content) } else { CallToolResult::success(content) }
        }
        Err(e) => super::tool_error(e.to_string()),
    }
}

/// `send` without asking anyone: a refused reach is the result.
#[cfg(test)]
pub fn run(params: Params, ctx: &dyn AxelContext) -> Result<CallToolResult, rmcp::ErrorData> {
    let (to, kind) = match validate(&params) {
        Ok(v) => v,
        Err(e) => return Ok(super::tool_error(e)),
    };
    Ok(finish(ctx.send(&to, &params.text, kind, crate::context::Grant::None)))
}

/// The owner's answer when an agent may not reach another (MCP elicitation).
/// The choices are an inline string `enum`: elicitation forms take flat
/// primitive fields only, no `$ref`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReachAnswer {
    #[schemars(description = "Let the agent send this message?", extend("enum" = ["Allow this message", "Always allow", "Don't allow"]))]
    pub answer: String,
}

/// The choices, as the owner reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReachChoice {
    Once,
    Always,
    No,
}

impl ReachChoice {
    /// From the label picked; anything else is a refusal.
    pub fn parse(label: &str) -> ReachChoice {
        match label.trim() {
            "Allow this message" => ReachChoice::Once,
            "Always allow" => ReachChoice::Always,
            _ => ReachChoice::No,
        }
    }
}

rmcp::elicit_safe!(ReachAnswer);

/// What the owner was asked when `from` may not reach `to`.
pub fn reach_question(from: &str, to: &[String]) -> String {
    format!("{from} wants to message {} on this thread, but isn't allowed to reach {}. Allow it?", to.join(", "), if to.len() == 1 { "it" } else { "them" })
}

/// The grant an answer gives, or `None` to refuse.
pub fn grant_for(choice: ReachChoice) -> Option<crate::context::Grant> {
    match choice {
        ReachChoice::Once => Some(crate::context::Grant::Once),
        ReachChoice::Always => Some(crate::context::Grant::Always),
        ReachChoice::No => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{CallScope, Delivery, Grant, MessageKind, Outcome, SendError, SendReport};
    use std::sync::Mutex;

    struct Fake(Mutex<Vec<(Vec<String>, String, MessageKind)>>);
    impl AxelContext for Fake {
        fn scope(&self) -> Result<CallScope, rmcp::ErrorData> {
            Err(rmcp::ErrorData::internal_error("no store", None))
        }
        fn send(&self, to: &[String], text: &str, kind: MessageKind, grant: Grant) -> Result<SendReport, SendError> {
            self.0.lock().unwrap().push((to.to_vec(), text.to_string(), kind));
            if to.iter().any(|t| t == "nobody") && grant == Grant::None {
                return Err(SendError::NoReach { from: "nova".into(), to: vec!["nobody".into()] });
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
        let r = run(Params { to: vec!["@atlas".into(), " ".into()], text: "review src/auth.rs".into(), fyi: false }, &fake).unwrap();
        assert_eq!(fake.0.lock().unwrap()[0].0, vec!["atlas".to_string()]);
        assert_eq!(fake.0.lock().unwrap()[0].2, MessageKind::Request);
        assert_ne!(r.is_error, Some(true));
        let text = &r.content[0].as_text().unwrap().text;
        assert!(text.contains("\"outcome\": \"queued\"") && text.contains("\"to\": \"atlas\""), "{text}");
    }

    #[test]
    fn refusals_are_tool_errors_with_the_reason() {
        let fake = Fake(Mutex::new(Vec::new()));
        let r = run(Params { to: vec!["nobody".into()], text: "hi".into(), fyi: false }, &fake).unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(r.content[0].as_text().unwrap().text.contains("axel agent edit"));
        let r = run(Params { to: vec![], text: "hi".into(), fyi: false }, &fake).unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[test]
    fn the_owner_is_asked_in_plain_words_and_the_answer_maps_to_a_grant() {
        assert_eq!(reach_question("jude", &["atlas".into()]), "jude wants to message atlas on this thread, but isn't allowed to reach it. Allow it?");
        let a: ReachAnswer = serde_json::from_value(serde_json::json!({ "answer": "Always allow" })).unwrap();
        assert_eq!(grant_for(ReachChoice::parse(&a.answer)), Some(Grant::Always));
        assert_eq!(ReachChoice::parse("something else"), ReachChoice::No, "only an explicit yes grants");
        assert_eq!(grant_for(ReachChoice::Once), Some(Grant::Once));
        assert_eq!(grant_for(ReachChoice::No), None);
        // The form the client renders: one field, a choice among the three labels.
        let form = serde_json::to_value(rmcp::model::ElicitationSchema::from_type::<ReachAnswer>().expect("a valid elicitation form")).unwrap();
        assert_eq!(form["properties"]["answer"]["enum"], serde_json::json!(["Allow this message", "Always allow", "Don't allow"]), "{form}");
    }

    #[test]
    fn fyi_is_passed_on_and_defaults_to_a_request() {
        let fake = Fake(Mutex::new(Vec::new()));
        run(Params { to: vec!["atlas".into()], text: "FYI: tests pass".into(), fyi: true }, &fake).unwrap();
        assert_eq!(fake.0.lock().unwrap()[0].2, MessageKind::Fyi);
        let p: Params = serde_json::from_value(serde_json::json!({ "to": ["atlas"], "text": "review this" })).unwrap();
        assert!(!p.fyi, "leaving fyi out is a request");
    }
}
