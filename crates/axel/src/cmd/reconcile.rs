//! Reconcile projections with reality: a live session whose Rex pane no longer
//! exists is ended. Cheap (one `session.view`), called when the UI opens and
//! when threads are listed.

use anyhow::Result;
use axel_store::{SessionFilter, SessionState};

use super::Ctx;

pub fn run(ctx: &Ctx) -> Result<usize> {
    let Some(rex) = &ctx.rex else { return Ok(0) };
    let live = ctx.store.list_sessions(SessionFilter {
        states: Some(vec![SessionState::Starting, SessionState::Running, SessionState::Blocked, SessionState::Idle]),
        ..Default::default()
    })?;
    // Only this machine's sessions: a remote host's Rex ids mean nothing here.
    let local = ctx.store.local_environment()?.id;
    let ours: Vec<_> = live
        .into_iter()
        .filter(|s| s.environment_id == local)
        .filter(|s| s.rex_session_id.as_deref() == Some(rex.env().session.as_str()) && s.rex_block_id.is_some())
        .collect();
    if ours.is_empty() {
        return Ok(0);
    }
    // Every block Rex has, placed or detached: an agent's session pane is
    // detached (minimized) and still alive.
    let listed = rex.api_call("session.list_blocks", serde_json::json!({}))?;
    let blocks: std::collections::HashSet<String> = listed
        .get("blocks")
        .and_then(|b| b.as_array())
        .into_iter()
        .flatten()
        .filter_map(|b| b.get("block_id").and_then(|i| i.as_str()).map(String::from))
        .collect();
    let owner = ctx.owner()?;
    let mut ended = 0;
    for s in ours {
        if !blocks.contains(s.rex_block_id.as_deref().unwrap_or("")) {
            ctx.store.end_session(&owner, &s.id, Some("pane closed"))?;
            ended += 1;
        }
    }
    Ok(ended)
}
