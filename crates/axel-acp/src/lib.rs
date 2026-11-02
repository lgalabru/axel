//! axel-acp: the managed runtime.
//!
//! Axel drives a coding harness over **ACP** (Agent Client Protocol, JSON-RPC
//! over stdio) and owns the conversation.  [`run_host`] hosts one managed
//! session: it spawns the adapter ([`adapter_command`]), creates the ACP
//! session, serves commands on a unix socket ([`socket`]), answers permission
//! requests by [`policy`], projects every `session/update` into
//! `conversation_items` in the store, records a signed `turn` per completed
//! prompt, logs every frame, and prints one coloured line per event to the
//! host pane.  See docs/DESIGN.md "Managed runtime (ACP)" and
//! docs/CONTRACTS.md "axel-acp".

mod adapter;
mod agent_rpc;
pub mod claude;
pub mod codex;
pub mod attach;
mod host;
pub mod log;
pub mod policy;
pub mod socket;
mod transport;

pub use adapter::{adapter_command, env_var_name, shell_split};
pub use host::{HostConfig, McpStdio, resolve_model, run_host};
