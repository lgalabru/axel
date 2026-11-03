//! Command dispatch and shared context.

mod agent;
mod convert;
mod doctor;
mod hook;
mod host;
mod inbox;
mod launch;
mod log;
mod policy;
mod reconcile;
mod session;
mod setup;
mod skills;
mod links;
mod status;
mod thread;
mod ui;
mod usage;
mod mcp;

use anyhow::{Context, Result};
use axel_identity::Keystore;
use axel_store::Store;

use crate::cli::{Cli, Command};

/// Everything a command needs: the store, the keystore, and Rex if present.
pub struct Ctx {
    pub store: Store,
    pub keystore: Keystore,
    pub rex: Option<axel_rex::Rex>,
}

impl Ctx {
    pub fn open() -> Result<Ctx> {
        let store = Store::open_default().context("opening ~/.axel/axel.db")?;
        let keystore = Keystore::open().context("opening keystore")?;
        let rex = axel_rex::detect().map(axel_rex::Rex::new);
        Ok(Ctx { store, keystore, rex })
    }

    /// Owner keypair, created on first use.  On a remote host this is also
    /// the host key that agents delegate to.
    pub fn owner(&self) -> Result<axel_identity::Keypair> {
        self.keystore.owner().context("loading owner key")
    }

    /// The key that signs for `agent` on `thread`: the agent's own key when
    /// it lives here, else this host's key when it holds the agent's grant
    /// for that thread (the agent works here remotely; the store attaches
    /// the grant when signing).
    pub fn agent_signer(&self, agent: &str, thread: Option<&str>) -> Result<axel_identity::Keypair> {
        if let Some(kp) = self.keystore.agent(&agent.parse()?)? {
            return Ok(kp);
        }
        // A job run here for another machine: its own key, delegated by the agent.
        if let Some(t) = thread
            && let Some(pk) = self.store.delegate_signer(agent, t)?
            && let Some(kp) = self.keystore.agent(&pk.parse()?)?
        {
            return Ok(kp);
        }
        anyhow::bail!("no key for agent {agent} here, and no delegation from it for this thread")
    }
}

pub fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        None => {
            // The first time, with no agent yet: walk through setup, then open.
            if setup::interactive() && Ctx::open().map(|c| setup::needed(&c)).unwrap_or(false) {
                return match setup::run()? {
                    setup::Next::Open => ui::open_launcher(),
                    setup::Next::Quit => Ok(()),
                };
            }
            ui::open_launcher()
        }
        Some(Command::ClaudeAcp) => axel_acp::claude::serve_stdio(),
        Some(Command::CodexAcp) => axel_acp::codex::serve_stdio(),
        Some(Command::Setup) => match setup::run()? {
            setup::Next::Open => ui::open_launcher(),
            setup::Next::Quit => Ok(()),
        },
        Some(Command::Ui { screen, thread }) => ui::run_inline(screen, thread),
        Some(Command::Session { command }) => session::run(command),
        Some(Command::Log { session, follow }) => log::run(&session, follow),
        Some(Command::Agent { command }) => agent::run(command),
        Some(Command::Skills { command }) => skills::run(command),
        Some(Command::Thread { command }) => thread::run(command),
        Some(Command::Hook { harness, guard_only }) => hook::run(harness, guard_only),
        Some(Command::Status { state, msg, kind }) => status::run(&state, msg.as_deref(), kind.as_deref()),
        Some(Command::Send { text, fyi, to, reply }) => {
            let kind = match (fyi, reply) {
                (true, _) => axel_mcp::MessageKind::Fyi,
                (_, true) => axel_mcp::MessageKind::Reply,
                _ => axel_mcp::MessageKind::Request,
            };
            inbox::send(&text.join(" "), &to, kind)
        }
        Some(Command::Inbox { agent, json }) => inbox::show(agent.as_deref(), json),
        Some(Command::Doctor) => doctor::run(),
        Some(Command::Mcp) => mcp::run(),
        Some(Command::Links { command: crate::cli::LinksCommand::Install }) => {
            let app = links::install()?;
            println!("axel-links:// links open through {}", app.display());
            Ok(())
        }
        Some(Command::OpenUrl { url }) => links::open_url(&url),
        Some(Command::Host { command }) => match command {
            crate::cli::HostCommand::Add { label, ssh, install } => host::add(&label, ssh.as_deref(), install),
            crate::cli::HostCommand::Ls => host::list(),
            crate::cli::HostCommand::Rm { label } => host::remove(&label),
            crate::cli::HostCommand::Rpc => host::rpc_serve(),
        },
        Some(Command::Sync { host, follow }) => host::sync_cmd(host.as_deref(), follow),
    }
}
