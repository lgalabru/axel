//! A thread as the web pane renders it: [`axel_tui::model::ThreadData`],
//! the very model the terminal view draws, written out as JSON.  The page
//! never reads the store itself, so the two views can't disagree.

use axel_tui::model::{ChoiceOption, ItemBody, ItemKind, ItemStatus, LiveSession, ThreadData, ThreadItem};
use serde_json::{Value, json};

/// The most items a page is sent: the newest ones.  A thread's whole day
/// of work fits; past it the page says how many it left out.
pub const ITEMS_MAX: usize = 2000;

/// The thread, for the page.
pub fn thread(data: &ThreadData) -> Value {
    let skipped = data.items.len().saturating_sub(ITEMS_MAX);
    let items: Vec<Value> = data.items[skipped..].iter().map(item).collect();
    debug_assert!(items.len() <= ITEMS_MAX);
    json!({
        "id": data.thread_id,
        "title": data.title,
        "agents": data.agents.iter().map(|(name, color, state)| json!({
            "name": name,
            "color": color.map(|c| c.as_str()),
            "state": state.as_str(),
        })).collect::<Vec<_>>(),
        "known_agents": data.known_agents.iter().map(|(name, color)| json!({ "name": name, "color": color.map(|c| c.as_str()) })).collect::<Vec<_>>(),
        "environments": data.environments,
        "tokens": data.tokens,
        "cost_usd": data.cost_usd,
        "live": data.live_sessions.iter().map(live).collect::<Vec<_>>(),
        "items": items,
        "skipped": skipped,
    })
}

fn live(s: &LiveSession) -> Value {
    json!({
        "id": s.id,
        "agent": s.agent_name,
        "managed": s.managed,
        "mode": s.mode.as_str(),
        "host": s.host,
    })
}

fn item(it: &ThreadItem) -> Value {
    json!({
        "id": it.id,
        "session_id": it.session_id,
        "agent": it.agent.as_ref().map(|(name, color)| json!({ "name": name, "color": color.map(|c| c.as_str()) })),
        "author": it.author,
        "turn": it.turn,
        "at_ms": it.at_ms,
        "kind": kind(it.kind),
        "status": it.status.map(status),
        "steering": it.steering,
        "relay_to": it.relay_to,
        "attachments": it.attachments.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "body": body(&it.body),
    })
}

fn body(b: &ItemBody) -> Value {
    match b {
        ItemBody::Text(text) => json!({ "type": "text", "text": text }),
        ItemBody::Tool { title, kind, input, output, summary, .. } => json!({
            "type": "tool", "title": title, "kind": kind, "input": input, "output": output, "summary": summary,
        }),
        ItemBody::Choice { prompt, options, multiple, chosen } => json!({
            "type": "choice",
            "prompt": prompt,
            "options": options.iter().map(option).collect::<Vec<_>>(),
            "multiple": multiple.map(|(min, max)| json!({ "min": min, "max": max })),
            "chosen": chosen,
        }),
        ItemBody::Answered(label) => json!({ "type": "answered", "label": label }),
        ItemBody::Changes { files, authored, added, removed, between_turns } => json!({
            "type": "changes", "files": files, "authored": authored, "added": added, "removed": removed, "between_turns": between_turns,
        }),
        ItemBody::Empty => json!({ "type": "empty" }),
    }
}

fn option(o: &ChoiceOption) -> Value {
    json!({ "id": o.id, "label": o.label, "exclusive": o.exclusive })
}

/// The wire names, the same as the store's (`axel_store::ItemKind`).
fn kind(k: ItemKind) -> &'static str {
    match k {
        ItemKind::UserMessage => "user_message",
        ItemKind::AssistantMessage => "assistant_message",
        ItemKind::Thought => "thought",
        ItemKind::ToolCall => "tool_call",
        ItemKind::Permission => "permission",
        ItemKind::Question => "question",
        ItemKind::Answer => "answer",
        ItemKind::Error => "error",
        ItemKind::TurnEnd => "turn_end",
    }
}

fn status(s: ItemStatus) -> &'static str {
    match s {
        ItemStatus::Pending => "pending",
        ItemStatus::Running => "running",
        ItemStatus::Completed => "completed",
        ItemStatus::Failed => "failed",
        ItemStatus::Cancelled => "cancelled",
        ItemStatus::Allowed => "allowed",
        ItemStatus::Denied => "denied",
        ItemStatus::Answered => "answered",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_item(id: &str, kind: ItemKind, text: &str) -> ThreadItem {
        ThreadItem {
            id: id.into(),
            session_id: "S1".into(),
            agent: (kind != ItemKind::UserMessage).then(|| ("atlas".to_string(), Some(axel_store::AgentColor::Orange))),
            author: None,
            seq: 0,
            turn: 1,
            at_ms: 1_700_000_000_000,
            kind,
            status: None,
            body: ItemBody::Text(text.into()),
            attachments: vec![],
            relay_to: None,
            steering: false,
        }
    }

    #[test]
    fn the_page_gets_the_model_the_terminal_draws() {
        let data = ThreadData {
            thread_id: "T1".into(),
            title: "Add retries".into(),
            agents: vec![("atlas".into(), Some(axel_store::AgentColor::Orange), axel_store::StatusState::Working)],
            items: vec![text_item("i1", ItemKind::UserMessage, "add retries"), text_item("i2", ItemKind::AssistantMessage, "<b>on it</b>")],
            ..Default::default()
        };
        let v = thread(&data);
        assert_eq!(v["title"], "Add retries");
        assert_eq!(v["agents"][0]["state"], "working");
        assert_eq!(v["items"][1]["kind"], "assistant_message");
        assert_eq!(v["items"][1]["body"]["text"], "<b>on it</b>", "text goes as text: the page escapes it");
        assert_eq!(v["items"][1]["agent"]["color"], "orange");
        assert_eq!(v["items"][0]["agent"], Value::Null, "the owner's own message");
    }

    #[test]
    fn a_long_thread_sends_only_its_newest_items() {
        let items: Vec<ThreadItem> = (0..ITEMS_MAX + 5).map(|i| text_item(&format!("i{i}"), ItemKind::AssistantMessage, "x")).collect();
        let v = thread(&ThreadData { items, ..Default::default() });
        assert_eq!(v["items"].as_array().unwrap().len(), ITEMS_MAX);
        assert_eq!(v["skipped"], 5);
        assert_eq!(v["items"][0]["id"], "i5");
    }
}
