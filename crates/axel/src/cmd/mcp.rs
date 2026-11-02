//! `axel mcp` — the MCP server on stdio.  Inside a session (`$AXEL_AGENT`
//! set) it acts as that agent: `send` and `set_status` run the same code as
//! `axel send` and `axel status`, under the same policy.

use std::sync::Arc;

use anyhow::Result;
use axel_mcp::{AxelContext, BlockedKind, CallScope, Grant, InboxMessage, LocalContext, MessageKind, SendError, SendReport, StatusState};

use super::Ctx;

struct CliContext;

impl AxelContext for CliContext {
    fn scope(&self) -> Result<CallScope, axel_mcp::ErrorData> {
        LocalContext.scope()
    }

    fn send(&self, to: &[String], text: &str, kind: MessageKind, grant: Grant) -> Result<SendReport, SendError> {
        let failed = |e: anyhow::Error| SendError::Failed(format!("{e:#}"));
        let ctx = Ctx::open().map_err(failed)?;
        let (recipients, hosts) = super::inbox::resolve_names(&ctx, to).map_err(failed)?;
        let mut _once = None;
        if let Some(from) = super::policy::caller_agent() {
            let denied = super::policy::denied_reach(&ctx, &from, &recipients).map_err(failed)?;
            if !denied.is_empty() {
                match grant {
                    // Ask the owner (the tool elicits): say who, by name.
                    Grant::None => {
                        let names = denied.iter().map(|r| super::policy::name_of(&ctx, r)).collect();
                        return Err(SendError::NoReach { from: super::policy::name_of(&ctx, &from), to: names });
                    }
                    // This message (and the summons it may need), nothing after.
                    Grant::Once => _once = Some(super::policy::grant_reach_once(&denied)),
                    // As `axel agent edit <from> --allow <them>`, which the owner just chose.
                    Grant::Always => {
                        let mut p = ctx.store.agent_policy(&from).map_err(|e| failed(e.into()))?;
                        for r in &denied {
                            p.reach.allow(r);
                        }
                        ctx.store.set_agent_policy(&from, &p).map_err(|e| failed(e.into()))?;
                    }
                }
            }
        }
        super::inbox::send_message(&ctx, &recipients, &hosts, text, kind).map_err(failed)
    }

    fn set_status(&self, state: StatusState, msg: Option<&str>, kind: Option<BlockedKind>) -> Result<(), String> {
        super::status::run(state.as_str(), msg, kind.map(BlockedKind::as_str)).map_err(|e| format!("{e:#}"))
    }

    fn inbox(&self, limit: usize) -> Result<Vec<InboxMessage>, String> {
        let go = || -> Result<Vec<InboxMessage>> { super::inbox::inbox_messages(&Ctx::open()?, limit) };
        go().map_err(|e| format!("{e:#}"))
    }
}

pub fn run() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(axel_mcp::run_server_with(Arc::new(CliContext))).map_err(|e| anyhow::anyhow!("{e}"))
}
