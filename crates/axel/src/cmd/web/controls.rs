//! The thread controls (Ctrl+D) as a web page: what the terminal pane
//! shows ([`ManageData`]) and the menu it offers ([`ManageView`]), written
//! out as JSON, and the outcome the page picked, read back.  The page only
//! walks the steps; what each action does, and the question asked before
//! it, stay in `manage.rs` and `axel_tui::manage`.

use axel_tui::manage::{Action, ManageData, ManageView, Outcome};
use serde_json::{Value, json};

/// The pane's data and menu, for the page.
pub fn controls(data: &ManageData) -> Value {
    let view = ManageView::new(data.clone());
    let actions: Vec<Value> = view
        .actions()
        .into_iter()
        .map(|a| json!({ "action": action_name(a), "key": a.key().to_string(), "label": a.label(), "about": view.describe(a), "danger": a.is_destructive() }))
        .collect();
    let agent_actions: serde_json::Map<String, Value> = data
        .agents
        .iter()
        .map(|agent| {
            let options: Vec<Value> = view.agent_actions(agent).into_iter().map(|(label, key, o)| json!({ "label": label, "key": key.to_string(), "outcome": outcome_json(&o) })).collect();
            (agent.clone(), Value::Array(options))
        })
        .collect();
    json!({
        "view": "controls",
        "id": data.thread_id,
        "title": data.title,
        "state": data.state,
        "age": data.age,
        "projects": data.projects,
        "sessions": data.sessions.iter().map(|s| json!({
            "agent": s.agent,
            "color": s.color.map(|c| c.as_str()),
            "runs": s.runs,
            "state": s.state,
            "live": s.live,
            "host": s.host,
            "turns": s.turns,
            "tool_calls": s.tool_calls,
            "tokens": s.tokens,
            "cost_usd": s.cost_usd,
            "context": s.context.map(|(used, size)| json!({ "used": used, "size": size })),
            "duration": axel_tui::manage::duration_label(s.duration_secs),
        })).collect::<Vec<_>>(),
        "spark": data.spark,
        "tokens": data.tokens,
        "cost_usd": data.cost_usd,
        "turns": data.turns,
        "tool_calls": data.tool_calls,
        "files_changed": data.files_changed,
        "agents": data.agents,
        "hosts": data.hosts,
        "note": data.note,
        "actions": actions,
        "agent_actions": agent_actions,
    })
}

fn action_name(a: Action) -> &'static str {
    match a {
        Action::Reset => "reset",
        Action::Move => "move",
        Action::Settle => "settle",
        Action::Reopen => "reopen",
        Action::Archive => "archive",
        Action::Stop => "stop",
        Action::StopAgent => "agents",
        Action::Rename => "rename",
        Action::HardReset => "hard_reset",
    }
}

/// An outcome as the page sends it back: `{"op": "reset", "agent": "atlas"}`.
pub fn outcome_json(o: &Outcome) -> Value {
    match o {
        Outcome::Close => json!({ "op": "close" }),
        Outcome::Reset { agent } => json!({ "op": "reset", "agent": agent }),
        Outcome::Move { agent, host } => json!({ "op": "move", "agent": agent, "host": host }),
        Outcome::Settle => json!({ "op": "settle" }),
        Outcome::Reopen => json!({ "op": "reopen" }),
        Outcome::Archive => json!({ "op": "archive" }),
        Outcome::Stop => json!({ "op": "stop" }),
        Outcome::StopAgent { agent } => json!({ "op": "stop_agent", "agent": agent }),
        Outcome::PauseAgent { agent } => json!({ "op": "pause_agent", "agent": agent }),
        Outcome::ResumeAgent { agent } => json!({ "op": "resume_agent", "agent": agent }),
        Outcome::RemoveAgent { agent } => json!({ "op": "remove_agent", "agent": agent }),
        Outcome::Rename(title) => json!({ "op": "rename", "title": title }),
        Outcome::HardReset => json!({ "op": "hard_reset" }),
    }
}

/// The outcome the page sent, when it is one; an agent or host it names
/// must be one the pane offered.
pub fn parse_outcome(v: &Value, data: &ManageData) -> Option<Outcome> {
    let text = |key: &str| v[key].as_str().map(str::to_string);
    let agent = || text("agent").filter(|a| data.agents.contains(a));
    Some(match v["op"].as_str()? {
        "close" => Outcome::Close,
        "reset" => Outcome::Reset { agent: agent()? },
        "move" => Outcome::Move { agent: agent()?, host: text("host").filter(|h| data.hosts.contains(h))? },
        "settle" => Outcome::Settle,
        "reopen" => Outcome::Reopen,
        "archive" => Outcome::Archive,
        "stop" => Outcome::Stop,
        "stop_agent" => Outcome::StopAgent { agent: agent()? },
        "pause_agent" => Outcome::PauseAgent { agent: agent()? },
        "resume_agent" => Outcome::ResumeAgent { agent: agent()? },
        "remove_agent" => Outcome::RemoveAgent { agent: agent()? },
        "rename" => Outcome::Rename(text("title").map(|t| t.trim().to_string()).filter(|t| !t.is_empty())?),
        "hard_reset" => Outcome::HardReset,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> ManageData {
        ManageData {
            thread_id: "T1".into(),
            title: "Ship it".into(),
            state: "open".into(),
            agents: vec!["atlas".into()],
            hosts: vec!["mini".into()],
            ..ManageData::default()
        }
    }

    #[test]
    fn every_outcome_the_page_is_given_reads_back_as_itself() {
        let d = data();
        for o in [
            Outcome::Close,
            Outcome::Reset { agent: "atlas".into() },
            Outcome::Move { agent: "atlas".into(), host: "mini".into() },
            Outcome::Settle,
            Outcome::Reopen,
            Outcome::Archive,
            Outcome::Stop,
            Outcome::StopAgent { agent: "atlas".into() },
            Outcome::PauseAgent { agent: "atlas".into() },
            Outcome::ResumeAgent { agent: "atlas".into() },
            Outcome::RemoveAgent { agent: "atlas".into() },
            Outcome::Rename("New title".into()),
            Outcome::HardReset,
        ] {
            assert_eq!(parse_outcome(&outcome_json(&o), &d), Some(o));
        }
    }

    #[test]
    fn the_page_cant_name_an_agent_or_host_the_pane_didnt_offer() {
        let d = data();
        assert_eq!(parse_outcome(&json!({ "op": "reset", "agent": "mallory" }), &d), None);
        assert_eq!(parse_outcome(&json!({ "op": "move", "agent": "atlas", "host": "elsewhere" }), &d), None);
        assert_eq!(parse_outcome(&json!({ "op": "rename", "title": "  " }), &d), None);
        assert_eq!(parse_outcome(&json!({ "op": "format_disk" }), &d), None);
    }

    #[test]
    fn the_menu_is_the_terminal_panes() {
        let page = controls(&data());
        let actions: Vec<&str> = page["actions"].as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap()).collect();
        assert_eq!(actions, ["reset", "move", "settle", "archive", "agents", "rename", "hard_reset"]);
        assert_eq!(page["agent_actions"]["atlas"][0]["outcome"]["op"], "remove_agent", "nothing live: only remove");
        assert_eq!(page["actions"][6]["danger"], true);
    }
}
