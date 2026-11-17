//! Seed a throwaway database with the token inspector's sample requests, to
//! look at `axel ui --screen inspect` for real:
//!
//! ```sh
//! export HOME=$(mktemp -d) AXEL_KEYSTORE=file
//! cargo run --offline -p axel-tui --example seed_inspect -- "$HOME/.axel/axel.db"
//! ./target/debug/axel ui --screen inspect
//! ```
//!
//! The database path is a required argument and must not exist yet, so a
//! real `~/.axel/axel.db` is never written to.

use anyhow::{Context, Result, bail};
use axel_identity::Keypair;
use axel_store::{AgentColor, Harness, NewAgent, RuntimeMode, Store};
use axel_tui::inspect::fixtures;

fn main() -> Result<()> {
    let Some(path) = std::env::args_os().nth(1).map(std::path::PathBuf::from) else {
        bail!("usage: seed_inspect <path to a new axel.db>");
    };
    if path.exists() {
        bail!("{} exists: seed a fresh database only", path.display());
    }
    let store = Store::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let owner = Keypair::generate();
    let webhooks = store.create_thread(&owner, "webhook retries", "make webhook delivery retry with backoff", None)?;
    let review = store.create_thread(&owner, "review the pay-kit PR", "review the open pull request", None)?;
    let terra = agent(&store, "terra", Harness::Claude, AgentColor::Purple)?;
    let orion = agent(&store, "orion", Harness::Codex, AgentColor::Green)?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let requests = fixtures::requests(now_ms);
    for mut r in requests.clone() {
        r.agent_pubkey = if r.agent_pubkey == fixtures::TERRA { terra.clone() } else { orion.clone() };
        r.thread_id = if r.thread_id == fixtures::THREAD_WEBHOOKS { webhooks.id.clone() } else { review.id.clone() };
        store.record_model_request(&r)?;
    }
    println!("seeded {} requests into {}", requests.len(), path.display());
    println!("webhook retries thread: {}", webhooks.id);
    Ok(())
}

fn agent(store: &Store, name: &str, harness: Harness, color: AgentColor) -> Result<String> {
    let pubkey = Keypair::generate().public().to_string();
    store.insert_agent(&NewAgent {
        pubkey: pubkey.clone(),
        name: name.into(),
        harness,
        model: None,
        effort: None,
        skills: Vec::new(),
        prompt: None,
        color: Some(color),
        runtime_mode: RuntimeMode::Autopilot,
        auth_tag: None,
    })?;
    Ok(pubkey)
}
