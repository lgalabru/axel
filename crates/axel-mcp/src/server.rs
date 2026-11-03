//! MCP server — thin dispatch layer.
//!
//! Each tool's logic and params live in `tools/<name>.rs`. The server holds
//! an [`AxelContext`](crate::context::AxelContext) that resolves a
//! [`CallScope`](crate::context::CallScope) per call.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};

use crate::context::{AxelContext, LocalContext, SharedContext};
use crate::tools;

/// The Axel MCP server.
pub struct AxelMcp {
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    context: SharedContext,
}

impl Default for AxelMcp {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl AxelMcp {
    /// Create a server backed by the local machine's store and keystore.
    pub fn new() -> Self {
        Self::with_context(Arc::new(LocalContext))
    }

    /// Create a server with a custom context (for testing or hosted use).
    pub fn with_context(context: Arc<dyn AxelContext>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            context: SharedContext::new(context),
        }
    }

    #[tool(
        description = "Create a new Axel agent: generates an Ed25519 keypair, stores it in the \
            keystore, attests it with the owner key, and records the agent in the database. \
            Returns the agent's name, public key, and harness. The agent can then be used \
            with `axel thread new --agent <name>` or mentioned with @name in messages."
    )]
    fn create_agent(
        &self,
        Parameters(params): Parameters<tools::create_agent::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if self.context.get().caller_agent().is_some() {
            return Ok(tools::tool_error("Agents can't create agents; ask the owner to run `axel agent new`."));
        }
        let scope = self.context.scope()?;
        tools::create_agent::run(params, &scope)
    }

    #[tool(
        description = "Send a message to other Axel agents on this thread. Each recipient gets it in front of it: \
            as the next prompt of its live session (queued if it is mid-turn), by being summoned onto the thread \
            if it isn't on it, or in its inbox if it runs in a terminal pane. Returns one outcome per recipient. \
            Write `name@host` (e.g. \"atlas@mini\") to summon the agent on a remote host when the owner asks for that. \
            Their answer comes back to you by itself when their turn ends (as your next prompt; the owner sees it \
            too), so end your turn after sending a request. Writing @name in your reply does nothing. \
            Send only when you have a request, a result or an answer: never just to say you got a message, \
            thanks or \"on it\" (each message costs the recipient a turn). For news that needs no reply, set \
            fyi: it waits for the recipient's next turn instead of starting one, and never summons. A refusal names the command the owner can run to allow it; never retry \
            a refusal another way."
    )]
    async fn send(
        &self,
        Parameters(params): Parameters<tools::send::Params>,
        rq: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (to, kind) = match tools::send::validate(&params) {
            Ok(v) => v,
            Err(e) => return Ok(tools::tool_error(e)),
        };
        let ctx = self.context.get();
        let first = ctx.send(&to, &params.text, kind, crate::context::Grant::None);
        // Not allowed to reach them: ask the owner, when the client can ask.
        let Err(crate::context::SendError::NoReach { from, to: denied }) = &first else {
            return Ok(tools::send::finish(first));
        };
        let question = tools::send::reach_question(from, denied);
        match rq.peer.elicit::<tools::send::ReachAnswer>(question).await {
            Ok(Some(a)) => match tools::send::grant_for(tools::send::ReachChoice::parse(&a.answer)) {
                Some(grant) => Ok(tools::send::finish(ctx.send(&to, &params.text, kind, grant))),
                None => Ok(tools::tool_error(format!("The owner didn't allow {from} to message {}. Don't try another way.", denied.join(", ")))),
            },
            Err(rmcp::service::ElicitationError::UserDeclined | rmcp::service::ElicitationError::UserCancelled) | Ok(None) => {
                Ok(tools::tool_error(format!("The owner didn't allow {from} to message {}. Don't try another way.", denied.join(", "))))
            }
            // The client can't ask (or asking failed): the refusal, with the command to allow it.
            Err(_) => Ok(tools::send::finish(first)),
        }
    }

    #[tool(
        description = "Report this session's status on the owner's dashboard: working with a one-line summary, \
            blocked when only the owner can unblock you, done when the task is finished."
    )]
    fn set_status(&self, Parameters(params): Parameters<tools::set_status::Params>) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::set_status::run(params, self.context.get())
    }

    #[tool(
        description = "Read the newest messages addressed to you: notes other agents left while you ran in a pane \
            or were offline. Messages marked handled: true already reached you as prompts or notes on their thread: \
            they're history, not requests; don't act on them again."
    )]
    fn read_inbox(&self, Parameters(params): Parameters<tools::read_inbox::Params>) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::read_inbox::run(params, self.context.get())
    }

    #[tool(
        description = "List all registered Axel agents with their name, public key, harness, \
            model, runtime mode, and unread message count. By default only active (non-archived) \
            agents are shown."
    )]
    fn list_agents(
        &self,
        Parameters(params): Parameters<tools::list_agents::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let scope = self.context.scope()?;
        tools::list_agents::run(params, &scope)
    }

    #[tool(
        description = "Get full details for a single Axel agent by name or public key. \
            Returns the agent's configuration, keystore status, owner attestation validity, \
            and creation timestamp."
    )]
    fn get_agent(
        &self,
        Parameters(params): Parameters<tools::get_agent::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let scope = self.context.scope()?;
        tools::get_agent::run(params, &scope)
    }
}

/// MCP server instructions included in the `initialize` response.
const INSTRUCTIONS: &str = "\
Axel runs coding agents as named Ed25519 identities on threads (units of work shared \
with the human owner). When you run inside Axel, $AXEL_AGENT_NAME is your name and \
$AXEL_THREAD your thread.

- `send`: message other agents on this thread (delivers, queues or summons them)
- `set_status`: working / blocked / done, shown on the owner's dashboard
- `read_inbox`: messages left for you; `handled: true` ones already reached you as prompts
- `list_agents`, `get_agent`: who exists
- `create_agent`: owner only; agents can't create agents

Other agents only see what you `send` them, and only the owner sees your replies. \
Each turn starts with what you missed on the thread, if anything: other agents' replies \
and who else is working there.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AxelMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                rmcp::model::Implementation::new("axel", env!("CARGO_PKG_VERSION")),
            )
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ServerHandler;

    struct AsAgent;
    impl crate::context::AxelContext for AsAgent {
        fn scope(&self) -> Result<crate::CallScope, rmcp::ErrorData> {
            panic!("an agent's create_agent must not reach the store")
        }
        fn caller_agent(&self) -> Option<String> {
            Some("pk".into())
        }
    }

    #[test]
    fn agents_cannot_create_agents() {
        let mcp = AxelMcp::with_context(std::sync::Arc::new(AsAgent));
        let p: tools::create_agent::Params = serde_json::from_str(r#"{"name":"sneaky"}"#).unwrap();
        let r = mcp.create_agent(Parameters(p)).unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(r.content[0].as_text().unwrap().text.contains("owner"));
    }

    #[test]
    fn server_info_names_axel() {
        let info = AxelMcp::new().get_info();
        assert_eq!(info.server_info.name, "axel");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn server_info_has_instructions() {
        let mcp = AxelMcp::new();
        let info = mcp.get_info();
        assert!(info.instructions.is_some());
        let instructions = info.instructions.unwrap();
        assert!(instructions.contains("create_agent"));
        assert!(instructions.contains("send"));
        assert!(instructions.contains("list_agents"));
        assert!(instructions.contains("get_agent"));
    }
}
