//! axel-proxy: the per-session inspector proxy (ADR 0011).
//!
//! A managed session's host starts one [`Proxy`] and points its harness at
//! it (`ANTHROPIC_BASE_URL`, or a Codex model provider).  Every request is
//! forwarded unchanged to the provider fixed at start, over verified TLS,
//! and its response streams back as it arrives.  On the way the proxy
//! records metadata through a [`RequestSink`]: a breakdown of the request's
//! context by kind and size, the response's token usage, timings and
//! outcome.  No header value, body or prompt text is ever recorded.
//!
//! What it refuses, before any byte reaches a provider: peers off loopback,
//! a path without the session's secret or outside the allowlist, `CONNECT`
//! and absolute-form URIs, requests carrying `Origin` or a foreign `Host`,
//! and bodies over 32 MiB.

mod analyze;
mod observe;
mod record;
mod route;
mod server;
mod upstream;
#[cfg(test)]
mod tests;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

pub use upstream::OpenAiUpstream;

use route::Secret;
use server::{CONNECTIONS_MAX, NO_TURN, Shared};
use upstream::{Transport, Upstreams};

/// What a session's proxy serves, fixed when it starts.
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub session_id: String,
    pub thread_id: String,
    pub agent_pubkey: String,
    /// `None`: `https://api.anthropic.com`.  `Some`: the user's own
    /// `ANTHROPIC_BASE_URL`, which must be `https`.
    pub anthropic_upstream: Option<String>,
    pub openai_upstream: OpenAiUpstream,
}

/// Where request rows go.  Called when each request starts and again when it
/// ends, on the proxy's own tasks: it must not block for long.
pub trait RequestSink: Send + Sync + 'static {
    fn record(&self, request: &axel_store::ModelRequest);
}

/// A running proxy.  Dropping it (or [`Proxy::shutdown`]) stops the listener
/// and ends its connections.
pub struct Proxy {
    port: u16,
    secret: String,
    turn: Arc<AtomicU64>,
    shutdown: Option<watch::Sender<()>>,
    accept: Option<JoinHandle<()>>,
}

impl Proxy {
    /// Bind `127.0.0.1` on a port the OS picks and serve until dropped.
    /// Fails if an upstream isn't `https`, or nothing can be bound.
    pub async fn start(config: ProxyConfig, sink: Arc<dyn RequestSink>) -> anyhow::Result<Proxy> {
        let upstreams = Upstreams::new(config.anthropic_upstream.as_deref(), config.openai_upstream)?;
        Proxy::start_with(config, upstreams, Transport::HttpsOnly, sink).await
    }

    /// Against plain-HTTP upstreams: only tests, with a fake provider.
    #[cfg(test)]
    pub(crate) async fn start_for_test(config: ProxyConfig, upstreams: Upstreams, sink: Arc<dyn RequestSink>) -> anyhow::Result<Proxy> {
        Proxy::start_with(config, upstreams, Transport::PlainAllowed, sink).await
    }

    async fn start_with(config: ProxyConfig, upstreams: Upstreams, transport: Transport, sink: Arc<dyn RequestSink>) -> anyhow::Result<Proxy> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
        let address = listener.local_addr()?;
        assert!(address.ip().is_loopback(), "the proxy listens on loopback only");
        let secret = Secret::generate()?;
        let secret_hex = secret.as_str().to_string();
        let turn = Arc::new(AtomicU64::new(NO_TURN));
        let shared = Arc::new(Shared {
            secret,
            port: address.port(),
            upstreams,
            client: upstream::client(transport)?,
            sink,
            session_id: config.session_id,
            thread_id: config.thread_id,
            agent_pubkey: config.agent_pubkey,
            turn: turn.clone(),
            connections: Arc::new(Semaphore::new(CONNECTIONS_MAX)),
        });
        let (shutdown, shutdown_rx) = watch::channel(());
        let accept = tokio::spawn(server::accept_loop(listener, shared, shutdown_rx));
        Ok(Proxy { port: address.port(), secret: secret_hex, turn, shutdown: Some(shutdown), accept: Some(accept) })
    }

    /// `http://127.0.0.1:<port>/<secret>/anthropic`, for `ANTHROPIC_BASE_URL`.
    pub fn anthropic_base_url(&self) -> String {
        format!("http://127.0.0.1:{}/{}/anthropic", self.port, self.secret)
    }

    /// `http://127.0.0.1:<port>/<secret>/openai`, for a Codex model
    /// provider's `base_url`.
    pub fn openai_base_url(&self) -> String {
        format!("http://127.0.0.1:{}/{}/openai", self.port, self.secret)
    }

    /// The session's current turn, stamped on the requests that start from now.
    pub fn set_turn(&self, turn: Option<u32>) {
        self.turn.store(turn.map_or(NO_TURN, u64::from), Ordering::Relaxed);
    }

    /// Stop listening, end every connection, and wait for the listener to go.
    pub async fn shutdown(mut self) {
        self.shutdown.take();
        if let Some(accept) = self.accept.take() {
            // The accept loop only ends; a join error would be its panic,
            // already reported by the runtime.
            let _ = accept.await;
        }
    }

    #[cfg(test)]
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    #[cfg(test)]
    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        // Closing the channel tells the accept loop and every connection to stop.
        self.shutdown.take();
    }
}
