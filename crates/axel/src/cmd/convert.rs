//! Conversions between the store's enums and the enums of the crates it must
//! not depend on (`axel_rex::osc`, `axel_config`).  Both sides are foreign to
//! this binary, so the orphan rules leave plain functions rather than `From`.

use axel_config::HarnessKind;
use axel_rex::osc;
use axel_store::{BlockedKind, Harness, StatusState};

/// The store state a program status maps to; `clear` reads as idle.
pub fn status_state(s: osc::State) -> StatusState {
    match s {
        osc::State::Idle | osc::State::Clear => StatusState::Idle,
        osc::State::Working => StatusState::Working,
        osc::State::Done => StatusState::Done,
        osc::State::Blocked => StatusState::Blocked,
        osc::State::Error => StatusState::Error,
    }
}

pub fn blocked_kind(k: osc::Kind) -> BlockedKind {
    match k {
        osc::Kind::Permission => BlockedKind::Permission,
        osc::Kind::Question => BlockedKind::Question,
        osc::Kind::Auth => BlockedKind::Auth,
    }
}

pub fn osc_kind(k: BlockedKind) -> osc::Kind {
    match k {
        BlockedKind::Permission => osc::Kind::Permission,
        BlockedKind::Question => osc::Kind::Question,
        BlockedKind::Auth => osc::Kind::Auth,
    }
}

/// The config-side kind of a stored harness.  The pane launcher and skill
/// drivers know Claude, Codex and OpenCode; every other harness runs its CLI
/// as is there (`Custom`, with [`pane_command`]).
pub fn harness_kind(h: &Harness) -> HarnessKind {
    match h {
        Harness::Claude => HarnessKind::Claude,
        Harness::Codex => HarnessKind::Codex,
        Harness::Opencode => HarnessKind::Opencode,
        _ => HarnessKind::Custom,
    }
}

/// What a `Custom`-kind harness runs in a pane: the command, or the CLI of a
/// harness the pane launcher has no flags for.
pub fn pane_command(h: &Harness) -> Option<String> {
    match h {
        Harness::Claude | Harness::Codex | Harness::Opencode => None,
        Harness::Custom(cmd) => Some(cmd.clone()),
        other => Some(other.program().to_string()),
    }
}
