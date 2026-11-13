//! What an agent missed on its thread, put in front of its next prompt.
//!
//! Agents on one thread can't see each other's turns: only the owner reads
//! the whole thread.  So when the owner turns to an agent ("@nova what do you
//! think?"), the agent would answer blind.  Before each turn the host gathers
//! what happened on the thread since the agent's last one (other agents'
//! replies, the owner's messages to them, notes left for this agent) and
//! who else is working there, and the agent reads that first.  The thread
//! itself still shows the message as written.

/// Who said something on the thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Speaker {
    /// The owner, to the agent `to`.
    Owner { to: String },
    /// An agent's reply, which only the owner saw.
    Agent { name: String },
    /// An agent's message to another agent.
    Relay { from: String, to: String },
    /// A note left for this agent that needed no reply (`send` with `fyi`).
    Note { from: String },
}

/// One thing said on the thread, oldest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub speaker: Speaker,
    pub text: String,
}

/// Another agent with a live session on the thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    /// Working on a turn right now.
    pub working: bool,
    /// What it's on, from its status (the head of its current prompt).
    pub doing: Option<String>,
}

/// The newest entry is quoted up to this many characters: it is usually
/// what the owner's message refers to.
const NEWEST_CHARS: usize = 6000;
/// Older entries are quoted up to this many characters.
const OLDER_CHARS: usize = 500;
/// At most this many entries, the newest.
pub const MAX_ENTRIES: usize = 8;

/// The catch-up text, or `None` when there is nothing to say: no entries,
/// and either no peers or not the agent's first turn.
pub fn render(entries: &[Entry], peers: &[Peer], first_turn: bool) -> Option<String> {
    if entries.is_empty() && (peers.is_empty() || !first_turn) {
        return None;
    }
    let mut out = String::new();
    let skip = entries.len().saturating_sub(MAX_ENTRIES);
    let shown = &entries[skip..];
    if !shown.is_empty() {
        out.push_str(if first_turn {
            "You're joining a thread that's already under way. Other agents' turns are visible only to the owner, so here is the recent conversation:\n"
        } else {
            "Since your last turn on this thread (other agents' turns are visible only to the owner, so here is what you missed):\n"
        });
        if skip > 0 {
            out.push_str(&format!("({skip} earlier left out)\n"));
        }
        for (i, e) in shown.iter().enumerate() {
            let limit = if i + 1 == shown.len() { NEWEST_CHARS } else { OLDER_CHARS };
            let who = match &e.speaker {
                Speaker::Owner { to } => format!("The owner, to {to}"),
                Speaker::Agent { name } => format!("{name} replied"),
                Speaker::Relay { from, to } => format!("{from}, to {to}"),
                Speaker::Note { from } => format!("{from} left you a note (no reply needed)"),
            };
            out.push_str(&format!("\n[{who}]\n{}\n", clip(e.text.trim(), limit)));
        }
    }
    if !peers.is_empty() {
        let list: Vec<String> = peers
            .iter()
            .map(|p| match (&p.doing, p.working) {
                (Some(d), true) => format!("{} (working on: {})", p.name, clip(d, 120)),
                (None, true) => format!("{} (working)", p.name),
                (_, false) => format!("{} (idle)", p.name),
            })
            .collect();
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "Also on this thread: {}. Don't redo their work: message them with `send` if you need it.\n",
            list.join(", ")
        ));
    }
    Some(out)
}

/// `catch_up` and the prompt, as the agent reads them.
pub fn prepend(catch_up: Option<&str>, prompt: &str) -> String {
    match catch_up {
        Some(c) => format!("{}\n---\n\n{prompt}", c.trim_end()),
        None => prompt.to_string(),
    }
}

/// At most `limit` characters, cut at a character boundary, with an ellipsis.
fn clip(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let mut out: String = s.chars().take(limit).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, text: &str) -> Entry {
        Entry { speaker: Speaker::Agent { name: name.into() }, text: text.into() }
    }

    #[test]
    fn nothing_to_say_is_none() {
        assert_eq!(render(&[], &[], true), None);
        let peer = Peer { name: "atlas".into(), working: false, doing: None };
        assert_eq!(render(&[], std::slice::from_ref(&peer), false), None, "peers alone only matter on the first turn");
        assert!(render(&[], &[peer], true).unwrap().contains("Also on this thread: atlas (idle)"));
    }

    #[test]
    fn the_reply_the_owner_refers_to_is_quoted_in_full() {
        let long = "x".repeat(3000);
        let entries = vec![
            Entry { speaker: Speaker::Owner { to: "atlas".into() }, text: "compare the two reports".into() },
            agent("atlas", &long),
        ];
        let c = render(&entries, &[], false).unwrap();
        assert!(c.starts_with("Since your last turn"));
        assert!(c.contains("[The owner, to atlas]\ncompare the two reports"));
        assert!(c.contains(&format!("[atlas replied]\n{long}")), "the newest entry isn't clipped at 3000 chars");
    }

    #[test]
    fn older_entries_are_clipped_and_only_the_newest_are_kept() {
        let mut entries: Vec<Entry> = (0..12).map(|i| agent("atlas", &format!("reply {i} {}", "y".repeat(800)))).collect();
        entries.push(Entry { speaker: Speaker::Note { from: "vega".into() }, text: "fyi: tests are green".into() });
        let c = render(&entries, &[], true).unwrap();
        assert!(c.starts_with("You're joining a thread"));
        assert!(c.contains("(5 earlier left out)"));
        assert!(!c.contains("reply 4 "), "only the last 8 entries");
        assert!(c.contains("reply 5 "));
        assert!(c.contains(&format!("reply 5 {}…", "y".repeat(OLDER_CHARS - "reply 5 ".len()))), "older ones clipped");
        assert!(c.contains("[vega left you a note (no reply needed)]\nfyi: tests are green"));
    }

    #[test]
    fn peers_say_what_they_are_on() {
        let peers = vec![
            Peer { name: "atlas".into(), working: true, doing: Some("review the QEDIT proofs".into()) },
            Peer { name: "vega".into(), working: true, doing: None },
        ];
        let c = render(&[agent("atlas", "done")], &peers, false).unwrap();
        assert!(c.contains("Also on this thread: atlas (working on: review the QEDIT proofs), vega (working)."));
        assert!(c.contains("Don't redo their work"));
    }

    #[test]
    fn the_prompt_comes_last() {
        assert_eq!(prepend(None, "hi"), "hi");
        assert_eq!(prepend(Some("caught up\n"), "@nova what do you think?"), "caught up\n---\n\n@nova what do you think?");
    }
}
