//! Message and thread-event kinds.
//!
//! Kinds are small integers on the wire; [`name`] gives the stable
//! human-readable label used in the DB and the TUI.
//!
//! - `< 20`: messages between principals ([`is_message`]).
//! - `20..100`: live records (`status`, `usage`).
//! - `>= 100`: thread event-log entries ([`is_thread_event`]); see
//!   `docs/DESIGN.md`, "Multiplayer: a thread is a signed event log".

/// A free-form note.
pub const NOTE: u32 = 1;
/// A task assigned to an agent.
pub const TASK: u32 = 2;
/// An agent asks the owner to allow a tool call.
pub const PERMISSION_REQUEST: u32 = 10;
/// The owner's reply to a [`PERMISSION_REQUEST`] (`"allow"` / `"deny"`).
pub const PERMISSION_REPLY: u32 = 11;
/// An agent asks a question.
pub const QUESTION: u32 = 12;
/// The reply to a [`QUESTION`].
pub const ANSWER: u32 = 13;
/// A status update from a running session (tags `s`).
pub const STATUS: u32 = 20;
/// A token/cost usage delta from a running session (tags `s`).
pub const USAGE: u32 = 21;
/// A context hand-off from one agent/thread to another.
pub const HANDOFF: u32 = 30;
/// One agent turn (a prompt/response round) within a session.
pub const TURN: u32 = 40;
/// Root of a thread's event log; its id is the thread id. Content = intent,
/// tag `title`.
pub const THREAD_CREATE: u32 = 100;
/// Thread scalar update, content JSON `{title?, state?}`.
pub const THREAD_UPDATE: u32 = 101;
/// Attach/detach a project to a thread.
pub const THREAD_PROJECT: u32 = 102;
/// Add/remove a participant (`["p", pubkey]`).
pub const THREAD_PARTICIPANT: u32 = 103;
/// A harness session started on some replica (tags `s`, `p`).
pub const SESSION_START: u32 = 110;
/// Session scalar update (tags `s`).
pub const SESSION_UPDATE: u32 = 111;
/// Session ended (tags `s`).
pub const SESSION_END: u32 = 112;

/// All known kinds, in numeric order.
pub const ALL: [u32; 17] = [
    NOTE,
    TASK,
    PERMISSION_REQUEST,
    PERMISSION_REPLY,
    QUESTION,
    ANSWER,
    STATUS,
    USAGE,
    HANDOFF,
    TURN,
    THREAD_CREATE,
    THREAD_UPDATE,
    THREAD_PROJECT,
    THREAD_PARTICIPANT,
    SESSION_START,
    SESSION_UPDATE,
    SESSION_END,
];

/// Stable snake_case name of a kind; `"unknown"` for anything else.
pub fn name(k: u32) -> &'static str {
    match k {
        NOTE => "note",
        TASK => "task",
        PERMISSION_REQUEST => "permission_request",
        PERMISSION_REPLY => "permission_reply",
        QUESTION => "question",
        ANSWER => "answer",
        STATUS => "status",
        USAGE => "usage",
        HANDOFF => "handoff",
        TURN => "turn",
        THREAD_CREATE => "thread.create",
        THREAD_UPDATE => "thread.update",
        THREAD_PROJECT => "thread.project",
        THREAD_PARTICIPANT => "thread.participant",
        SESSION_START => "session.start",
        SESSION_UPDATE => "session.update",
        SESSION_END => "session.end",
        _ => "unknown",
    }
}

/// Inverse of [`name`].
pub fn from_name(name: &str) -> Option<u32> {
    ALL.iter().copied().find(|&k| self::name(k) == name)
}

/// True for thread event-log kinds (`>= 100`): thread and session lifecycle.
pub fn is_thread_event(k: u32) -> bool {
    k >= 100
}

/// True for messages between principals (`< 20`): notes, tasks, requests
/// and replies. `status`/`usage`/`handoff` and thread events are not.
pub fn is_message(k: u32) -> bool {
    k < 20
}

/// True for kinds that block a thread until someone replies.
pub fn is_request(k: u32) -> bool {
    matches!(k, PERMISSION_REQUEST | QUESTION)
}

/// The kind that answers a request kind, if any.
pub fn reply_kind(k: u32) -> Option<u32> {
    match k {
        PERMISSION_REQUEST => Some(PERMISSION_REPLY),
        QUESTION => Some(ANSWER),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_roundtrip() {
        for k in ALL {
            assert_ne!(name(k), "unknown");
            assert_eq!(from_name(name(k)), Some(k));
        }
        assert_eq!(name(999), "unknown");
        assert_eq!(from_name("unknown"), None);
        assert_eq!(from_name("nope"), None);
    }

    #[test]
    fn requests_and_replies() {
        assert!(is_request(PERMISSION_REQUEST));
        assert!(is_request(QUESTION));
        assert!(!is_request(NOTE));
        assert_eq!(reply_kind(PERMISSION_REQUEST), Some(PERMISSION_REPLY));
        assert_eq!(reply_kind(QUESTION), Some(ANSWER));
        assert_eq!(reply_kind(STATUS), None);
    }

    #[test]
    fn event_log_kinds() {
        assert_eq!(name(USAGE), "usage");
        assert_eq!(name(THREAD_CREATE), "thread.create");
        assert_eq!(name(THREAD_UPDATE), "thread.update");
        assert_eq!(name(THREAD_PROJECT), "thread.project");
        assert_eq!(name(THREAD_PARTICIPANT), "thread.participant");
        assert_eq!(name(SESSION_START), "session.start");
        assert_eq!(name(SESSION_UPDATE), "session.update");
        assert_eq!(name(SESSION_END), "session.end");
        assert_eq!(from_name("session.end"), Some(112));
        assert_eq!(name(TURN), "turn");
        assert_eq!(from_name("turn"), Some(40));
        assert_eq!(ALL.len(), 17);
        let mut sorted = ALL;
        sorted.sort_unstable();
        assert_eq!(sorted, ALL, "ALL is in numeric order");
    }

    #[test]
    fn classification() {
        for k in [
            NOTE,
            TASK,
            PERMISSION_REQUEST,
            PERMISSION_REPLY,
            QUESTION,
            ANSWER,
        ] {
            assert!(is_message(k));
            assert!(!is_thread_event(k));
        }
        for k in [STATUS, USAGE, HANDOFF, TURN] {
            assert!(!is_message(k));
            assert!(!is_thread_event(k));
        }
        for k in [
            THREAD_CREATE,
            THREAD_UPDATE,
            THREAD_PROJECT,
            THREAD_PARTICIPANT,
            SESSION_START,
            SESSION_UPDATE,
            SESSION_END,
        ] {
            assert!(is_thread_event(k));
            assert!(!is_message(k));
        }
    }
}
