//! A proxy on its own, for trying a harness against it by hand:
//!
//! ```sh
//! cargo run -p axel-proxy --example smoke -- chatgpt /tmp/axel-smoke-urls
//! ANTHROPIC_BASE_URL=$(sed -n 1p /tmp/axel-smoke-urls) claude -p "say ok" --max-turns 1
//! ```
//!
//! The base URLs (they carry the session's secret) go to the file named, mode
//! 0600; each recorded row is printed as JSON, which holds no credential or
//! text by construction.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;

use axel_proxy::{OpenAiUpstream, Proxy, ProxyConfig, RequestSink};

struct Print;

impl RequestSink for Print {
    fn record(&self, request: &axel_store::ModelRequest) {
        println!("{}", serde_json::to_string(request).expect("a row serializes"));
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let openai_upstream = match args.next().as_deref() {
        Some("apikey") => OpenAiUpstream::ApiKey,
        _ => OpenAiUpstream::ChatGpt,
    };
    let urls_path = args.next().unwrap_or_else(|| "/tmp/axel-smoke-urls".into());
    let config = ProxyConfig {
        session_id: "smoke".into(),
        thread_id: "smoke".into(),
        agent_pubkey: "smoke".into(),
        anthropic_upstream: std::env::var("AXEL_SMOKE_ANTHROPIC_UPSTREAM").ok(),
        openai_upstream,
    };
    let proxy = Proxy::start(config, Arc::new(Print)).await?;
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&urls_path)?;
    writeln!(file, "{}\n{}", proxy.anthropic_base_url(), proxy.openai_base_url())?;
    eprintln!("proxy up; base URLs in {urls_path}; Ctrl-C to stop");
    tokio::signal::ctrl_c().await?;
    proxy.shutdown().await;
    Ok(())
}
