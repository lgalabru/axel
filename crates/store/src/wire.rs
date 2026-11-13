//! JSON content of the thread/session envelopes (docs/DESIGN.md "Multiplayer").
//!
//! These are the on-the-wire shapes; every field a replica might not know is
//! optional so older events keep parsing.

use serde::{Deserialize, Serialize};

use crate::types::lenient;
use crate::{
    Harness, ParticipantRole, RuntimeMode, SessionRuntime, SessionState, ThreadState, UsageDelta,
    WorkspaceKind,
};

/// Message kinds: `axel_inbox::kind` plus the ones the inbox crate does not
/// export yet.
pub mod kind {
    pub use axel_inbox::kind::{
        ANSWER, HANDOFF, NOTE, PERMISSION_REPLY, PERMISSION_REQUEST, QUESTION, SESSION_END,
        SESSION_START, SESSION_UPDATE, STATUS, TASK, THREAD_CREATE, THREAD_PARTICIPANT,
        THREAD_PROJECT, THREAD_UPDATE, USAGE,
    };
    /// A completed turn of a managed session (`["s", id]`, content = `TurnSummary`).
    pub const TURN: u32 = 40;

    /// Kinds that only make sense on a thread (must carry `["t", id]`, or be the root).
    pub fn is_thread_kind(k: u32) -> bool {
        matches!(
            k,
            STATUS | USAGE | TURN | THREAD_CREATE..=THREAD_PARTICIPANT | SESSION_START..=SESSION_END
        )
    }

    /// Kinds a replica accepts from its own registered agents without a participant check.
    pub fn is_own_agent_kind(k: u32) -> bool {
        matches!(k, STATUS | USAGE | TURN | SESSION_START..=SESSION_END)
    }

    /// Conversational kinds, shown by `thread_messages`: notes, tasks,
    /// requests and replies (everything below `STATUS`).
    pub fn is_message_kind(k: u32) -> bool {
        k < STATUS
    }

    /// Kinds whose `["p", pk]` tags address someone and belong in their inbox.
    pub fn is_addressed_kind(k: u32) -> bool {
        is_message_kind(k) || k == HANDOFF || k == THREAD_PARTICIPANT
    }
}

/// Tag names used by the store.
pub mod tag {
    pub const THREAD: &str = "t";
    pub const SESSION: &str = "s";
    pub const MENTION: &str = "p";
    pub const REF: &str = "e";
    pub const TITLE: &str = "title";
    /// `["parent", thread_id, relationship]`
    pub const PARENT: &str = "parent";
    /// `["auth", owner_b58, conditions, sig_hex]` (an owner attestation of the signer).
    pub const AUTH: &str = "auth";
    /// `["n", ulid]`: a nonce so identical deltas in one second get distinct ids.
    pub const NONCE: &str = "n";
}

/// kind 101 content.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ThreadState>,
}

/// kind 102 content.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadProjectOp {
    /// `attach | detach`
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_kind: Option<WorkspaceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
}

impl ThreadProjectOp {
    /// The element identity used for add/remove LWW: `repo_key`, else `path_hint`.
    pub fn key(&self) -> Option<&str> {
        self.repo_key
            .as_deref()
            .filter(|k| !k.is_empty())
            .or(self.path_hint.as_deref())
            .filter(|k| !k.is_empty())
    }
}

/// kind 103 content.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParticipantOp {
    /// `add | remove`
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<ParticipantRole>,
}

/// kind 110 content.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionStart {
    #[serde(default)]
    pub environment_id: String,
    #[serde(default)]
    pub environment_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_repo_key: Option<String>,
    /// Serialised as the builtin name or the custom command.
    #[serde(default)]
    pub harness: Harness,
    /// Unknown or empty text reads as the default mode.
    #[serde(default, deserialize_with = "lenient::or_default")]
    pub runtime_mode: RuntimeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    /// `pane | managed`; absent or unknown text reads as `pane`.
    #[serde(default, deserialize_with = "lenient::or_default")]
    pub runtime: SessionRuntime,
}

/// `rex` member of kind 111 content.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RexIds {
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub block: Option<String>,
}

/// kind 111 content.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionUpdateWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<SessionState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rex: Option<RexIds>,
    /// Unknown text reads as `None` (left unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient::or_none")]
    pub runtime_mode: Option<RuntimeMode>,
}

/// kind 112 content.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionEnd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// kind 21 content: a usage delta with the short names from the design table.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageWire {
    #[serde(default, alias = "input_tokens")]
    pub input: i64,
    #[serde(default, alias = "output_tokens")]
    pub output: i64,
    #[serde(default, alias = "cache_read_tokens")]
    pub cache_read: i64,
    #[serde(default, alias = "cache_write_tokens")]
    pub cache_write: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning: i64,
    #[serde(default)]
    pub turns: i64,
    #[serde(default)]
    pub cost_usd_est: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl From<&UsageDelta> for UsageWire {
    fn from(d: &UsageDelta) -> Self {
        UsageWire {
            input: d.input_tokens,
            output: d.output_tokens,
            cache_read: d.cache_read_tokens,
            cache_write: d.cache_write_tokens,
            reasoning: d.reasoning_tokens,
            turns: d.turns,
            cost_usd_est: d.cost_usd_est,
            model: d.model.clone(),
        }
    }
}

impl From<&UsageWire> for UsageDelta {
    fn from(w: &UsageWire) -> Self {
        UsageDelta {
            input_tokens: w.input,
            output_tokens: w.output,
            cache_read_tokens: w.cache_read,
            cache_write_tokens: w.cache_write,
            reasoning_tokens: w.reasoning,
            turns: w.turns,
            cost_usd_est: w.cost_usd_est,
            model: w.model.clone(),
        }
    }
}
