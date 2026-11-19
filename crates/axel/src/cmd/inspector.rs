//! The token inspector's proxy for a managed session (ADR 0011): started by
//! `axel session host` before the adapter, it sees every model request the
//! harness makes and records its metadata in the store.
//!
//! It runs on its own small runtime, so the host's blocking work (Git
//! checkpoints, store writes) never stalls a model stream, and records
//! through a dedicated thread with its own store connection (`Store` isn't
//! `Sync`), fed by a bounded channel: a full channel drops rows rather than
//! hold up a stream.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Context, Result};
use axel_i18n::t;
use axel_proxy::{OpenAiUpstream, Proxy, ProxyConfig, RequestSink, Throttle};
use axel_store::{Harness, ModelRequest, Store};

/// Rows waiting for the writer.  Each request makes two, a harness makes a
/// few per second at most, and a write takes about a millisecond: this
/// covers a store locked for seconds.
const ROWS_QUEUED_MAX: usize = 1024;
/// Worker threads for the proxy: streams wait on the network, not the CPU.
const PROXY_THREADS: usize = 2;
/// How long a stopping proxy's connections get to wind down.
const SHUTDOWN_TIMEOUT_MS: u64 = 1_000;
/// Largest `~/.codex/auth.json` read for its `auth_mode`: it holds a few tokens.
const CODEX_AUTH_BYTES_MAX: u64 = 1024 * 1024;

/// Whether the session runs through the inspector.
pub enum Inspection {
    /// The owner hasn't turned it on, or a harness the proxy doesn't speak for.
    Off,
    /// The proxy runs; the harness's environment points at it.
    On(Inspector),
    /// The proxy could not start: the session runs direct, and the thread
    /// is told why.
    Failed { notice: String },
}

/// A running proxy, with what it runs on.
pub struct Inspector {
    proxy: Arc<Proxy>,
    runtime: tokio::runtime::Runtime,
    writer: std::thread::JoinHandle<()>,
}

/// Who the session is, for the rows.
pub struct Session<'a> {
    pub session_id: &'a str,
    pub thread_id: &'a str,
    pub agent_pubkey: &'a str,
    pub harness: &'a Harness,
}

/// Whether this session should run through the inspector: only when the
/// owner asked.  `switched_on` is its thread's switch (`axel inspect on`);
/// `AXEL_INSPECT=1` turns it on for one session and `AXEL_INSPECT=0` off.
pub fn wanted(switched_on: bool) -> bool {
    wanted_with(switched_on, std::env::var("AXEL_INSPECT").ok().as_deref())
}

fn wanted_with(switched_on: bool, variable: Option<&str>) -> bool {
    match variable.map(str::trim) {
        Some("1") => true,
        Some("0") => false,
        _ => switched_on,
    }
}

/// A bad network the session replays, from `AXEL_THROTTLE`: a testing aid,
/// never on by default.  It implies the proxy, like `AXEL_INSPECT=1`.
pub struct NetworkThrottle {
    /// The profile as it was asked for (`airport`, `up=1mbit,rtt=300ms`).
    pub label: String,
    pub throttle: Throttle,
}

impl NetworkThrottle {
    /// What the thread is told: `network throttled: airport (1 Mbit/s up, …)`.
    pub fn notice(&self) -> String {
        t!("network throttled: {name} ({profile})", name = self.label, profile = self.throttle)
    }
}

/// The session's throttle, if `AXEL_THROTTLE` asks for one.  A malformed
/// profile is an error: a typo must never run the session unthrottled
/// while the owner believes otherwise.
pub fn throttle_from_env() -> Result<Option<NetworkThrottle>> {
    throttle_from(std::env::var("AXEL_THROTTLE").ok().as_deref())
}

fn throttle_from(variable: Option<&str>) -> Result<Option<NetworkThrottle>> {
    let Some(spec) = variable.map(str::trim).filter(|v| !v.is_empty()) else { return Ok(None) };
    let throttle = Throttle::parse(spec).with_context(|| t!("AXEL_THROTTLE is not a network profile"))?;
    Ok(Some(NetworkThrottle { label: spec.to_string(), throttle }))
}

/// Start the proxy for `session` when `wanted` or throttled, and point the
/// harness's environment `env` at it.  Never fails: a proxy that can't
/// start leaves `env` as it was.
pub fn start(session: &Session<'_>, wanted: bool, throttle: Option<Throttle>, env: &mut Vec<(String, String)>) -> Inspection {
    let is_proxied = matches!(session.harness, Harness::Claude | Harness::Codex);
    let is_wanted = wanted || throttle.is_some();
    if !is_wanted || !is_proxied {
        return Inspection::Off;
    }
    // The user's own Anthropic endpoint is Claude's business only.
    let anthropic_upstream = if *session.harness == Harness::Claude { user_anthropic_base_url(env) } else { None };
    match Inspector::start(session, anthropic_upstream, throttle) {
        Ok(inspector) => {
            inspector.point(session.harness, env);
            Inspection::On(inspector)
        }
        Err(e) => Inspection::Failed {
            notice: t!("the token inspector could not start, so this session talks to its provider directly: {error}", error = format!("{e:#}")),
        },
    }
}

impl Inspector {
    fn start(session: &Session<'_>, anthropic_upstream: Option<String>, throttle: Option<Throttle>) -> Result<Inspector> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(PROXY_THREADS)
            .thread_name("axel-inspector")
            .enable_all()
            .build()
            .context("starting the inspector's runtime")?;
        let (sink, writer) = StoreSink::spawn(Store::default_path())?;
        let config = ProxyConfig {
            session_id: session.session_id.to_string(),
            thread_id: session.thread_id.to_string(),
            agent_pubkey: session.agent_pubkey.to_string(),
            anthropic_upstream,
            openai_upstream: codex_upstream(),
            throttle,
        };
        // On failure the sink drops with the error, which ends the writer.
        let proxy = runtime.block_on(Proxy::start(config, Arc::new(sink)))?;
        Ok(Inspector { proxy: Arc::new(proxy), runtime, writer })
    }

    /// Point the harness at the proxy: Claude Code by `ANTHROPIC_BASE_URL`
    /// (replacing the user's own, which became the upstream), Axel's Codex
    /// adapter by `AXEL_CODEX_BASE_URL`, which it turns into `-c` flags.
    fn point(&self, harness: &Harness, env: &mut Vec<(String, String)>) {
        let (name, url) = match harness {
            Harness::Claude => ("ANTHROPIC_BASE_URL", self.proxy.anthropic_base_url()),
            Harness::Codex => ("AXEL_CODEX_BASE_URL", self.proxy.openai_base_url()),
            other => unreachable!("start() proxies only Claude and Codex, not {other:?}"),
        };
        set_env(env, name, url);
    }

    /// What the host calls as each turn starts.
    pub fn turn_hook(&self) -> axel_acp::TurnHook {
        let proxy = self.proxy.clone();
        axel_acp::TurnHook(Arc::new(move |turn| proxy.set_turn(Some(turn))))
    }

    /// Stop the proxy and wait for the last rows to be written.
    pub fn stop(self) {
        let Inspector { proxy, runtime, writer } = self;
        // The host has dropped its turn hook by now; if a clone lingers,
        // dropping the runtime below still ends every task.
        if let Ok(proxy) = Arc::try_unwrap(proxy) {
            runtime.block_on(proxy.shutdown());
        }
        runtime.shutdown_timeout(Duration::from_millis(SHUTDOWN_TIMEOUT_MS));
        // Every sender went with the runtime's tasks: the writer drains and ends.
        if writer.join().is_err() {
            eprintln!("axel: {}", t!("the inspector's writer stopped with an error"));
        }
    }
}

/// The user's own `ANTHROPIC_BASE_URL`, if they set one: the proxy forwards
/// there instead of to Anthropic (and refuses to start if it isn't https).
fn user_anthropic_base_url(env: &[(String, String)]) -> Option<String> {
    env.iter().find(|(k, _)| k == "ANTHROPIC_BASE_URL").map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Whether `url` is an inspector proxy's base URL
/// (`http://127.0.0.1:<port>/<64 hex>/<side>`), as opposed to one the user set.
pub fn is_proxy_url(url: &str) -> bool {
    let Some(rest) = url.trim().strip_prefix("http://127.0.0.1:") else { return false };
    let mut segments = rest.split('/');
    let (Some(port), Some(secret), Some(side), None) = (segments.next(), segments.next(), segments.next(), segments.next()) else { return false };
    let is_secret = secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit());
    port.parse::<u16>().is_ok() && is_secret && matches!(side, "anthropic" | "openai")
}

fn set_env(env: &mut Vec<(String, String)>, name: &str, value: String) {
    env.retain(|(k, _)| k != name);
    env.push((name.to_string(), value));
    debug_assert_eq!(env.iter().filter(|(k, _)| k == name).count(), 1);
}

/// Which OpenAI backend Codex's login uses, from `auth_mode` in its
/// `auth.json`.  Only that field is read into anything; the tokens beside
/// it are skipped by the parser and never kept.
fn codex_upstream() -> OpenAiUpstream {
    let home = std::env::var_os("CODEX_HOME").filter(|h| !h.is_empty()).map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".codex")));
    let Some(path) = home.map(|h| h.join("auth.json")) else { return OpenAiUpstream::ApiKey };
    codex_auth_mode(&path).map_or(OpenAiUpstream::ApiKey, |mode| parse_auth_mode(&mode))
}

fn codex_auth_mode(path: &std::path::Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Auth {
        auth_mode: Option<String>,
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(CODEX_AUTH_BYTES_MAX).read_to_end(&mut bytes).ok()?;
    let auth: Auth = serde_json::from_slice(&bytes).ok()?;
    auth.auth_mode
}

/// `chatgpt` (and Codex's other ChatGPT modes) or an API key.
fn parse_auth_mode(mode: &str) -> OpenAiUpstream {
    if mode.to_ascii_lowercase().starts_with("chatgpt") { OpenAiUpstream::ChatGpt } else { OpenAiUpstream::ApiKey }
}

/// Rows to the writer thread; never blocks.
struct StoreSink {
    rows: SyncSender<ModelRequest>,
    /// Said once that rows are being dropped, not once per row.
    has_told_full: AtomicBool,
}

impl StoreSink {
    /// The sink and its writer, which has its own connection to the store at
    /// `path`; an error if the store can't be opened.
    fn spawn(path: PathBuf) -> Result<(StoreSink, std::thread::JoinHandle<()>)> {
        let (rows, received) = mpsc::sync_channel::<ModelRequest>(ROWS_QUEUED_MAX);
        let (opened, is_open) = mpsc::sync_channel::<Result<(), String>>(1);
        let writer = std::thread::Builder::new()
            .name("axel-inspector-writer".into())
            .spawn(move || match Store::open(&path) {
                Ok(store) => {
                    // The receiver waits below; if it is gone, so is the host.
                    let _ = opened.send(Ok(()));
                    write_rows(&store, received);
                }
                Err(e) => {
                    let _ = opened.send(Err(e.to_string()));
                }
            })
            .context("starting the inspector's writer")?;
        match is_open.recv() {
            Ok(Ok(())) => Ok((StoreSink { rows, has_told_full: AtomicBool::new(false) }, writer)),
            Ok(Err(e)) => anyhow::bail!("{e}"),
            Err(_) => anyhow::bail!("the inspector's writer stopped before it opened the store"),
        }
    }
}

impl RequestSink for StoreSink {
    fn record(&self, request: &ModelRequest) {
        match self.rows.try_send(request.clone()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                if !self.has_told_full.swap(true, Ordering::Relaxed) {
                    eprintln!("axel: {}", t!("the token inspector is behind; some requests go unrecorded"));
                }
            }
            // The writer stopped (its error was said); the proxy goes on.
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

/// Write rows until every sender is gone.  A row the store refuses is said
/// once and skipped: the proxy must not stop over its own bookkeeping.
fn write_rows(store: &Store, rows: Receiver<ModelRequest>) {
    let mut has_told = false;
    for row in rows {
        if let Err(e) = store.record_model_request(&row)
            && !has_told
        {
            has_told = true;
            eprintln!("axel: {}", t!("the token inspector could not record a request: {error}", error = e));
        }
    }
}

/// `axel inspect on|off|status`: a thread's switch.  The thread is
/// `--thread`, else the one this runs in; `status` with neither lists the
/// inspected threads.  New managed sessions read the switch when they start;
/// running ones keep what they started with.
pub fn switch(ctx: &super::Ctx, switch: crate::cli::InspectSwitch, thread: Option<&str>) -> Result<()> {
    use crate::cli::InspectSwitch;
    let here = std::env::var("AXEL_THREAD").ok().filter(|t| !t.trim().is_empty());
    let asked = thread.map(str::to_string).or(here);
    let Some(asked) = asked else {
        if switch == InspectSwitch::Status {
            return list_inspected(ctx);
        }
        anyhow::bail!("{}", t!("which thread? Pass --thread <id>; inside a thread, its own is used."));
    };
    let id = super::ui::resolve_thread_id(ctx, &asked)?;
    let title = ctx.store.get_thread(&id)?.map(|t| t.title).unwrap_or_else(|| id.clone());
    match switch {
        InspectSwitch::On => {
            ctx.store.set_thread_inspected(&id, true)?;
            println!("{}", t!("The token inspector is on for {thread}. Its new managed sessions send their model requests through a local proxy on 127.0.0.1, which records their size, tokens and timing, never their prompts or credentials. Sessions already running are unchanged.", thread = title));
            println!("{}", t!("Open it with `axel inspect`; turn it off with `axel inspect off`."));
        }
        InspectSwitch::Off => {
            ctx.store.set_thread_inspected(&id, false)?;
            println!("{}", t!("The token inspector is off for {thread}. Its new managed sessions talk to their provider directly; sessions already running keep their proxy until they end.", thread = title));
        }
        InspectSwitch::Status => {
            if ctx.store.thread_inspected(&id)? {
                println!("{}", t!("The token inspector is on for {thread}: its new managed sessions are recorded.", thread = title));
            } else {
                println!("{}", t!("The token inspector is off for {thread}. Turn it on with `axel inspect on`.", thread = title));
            }
        }
    }
    Ok(())
}

/// Every inspected thread, one a line.
fn list_inspected(ctx: &super::Ctx) -> Result<()> {
    let ids = ctx.store.inspected_threads()?;
    if ids.is_empty() {
        println!("{}", t!("The token inspector is off for every thread. Turn it on for one with `axel inspect on --thread <id>`."));
        return Ok(());
    }
    if ids.len() == 1 {
        println!("{}", t!("The token inspector is on for 1 thread:"));
    } else {
        println!("{}", t!("The token inspector is on for {n} threads:", n = ids.len()));
    }
    for id in ids {
        let title = ctx.store.get_thread(&id)?.map(|t| t.title).unwrap_or_default();
        println!("  {}  {title}", id.chars().take(8).collect::<String>());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_proxy_runs_only_when_the_owner_asked() {
        assert!(!wanted_with(false, None), "off by default");
        assert!(wanted_with(true, None), "on after `axel inspect on`");
        assert!(wanted_with(false, Some("1")), "AXEL_INSPECT=1 for one session");
        assert!(!wanted_with(true, Some("0")), "AXEL_INSPECT=0 wins over the switch");
        assert!(!wanted_with(false, Some("yes")), "anything else leaves the switch as it is");
        assert!(wanted_with(true, Some("")));
    }

    #[test]
    fn a_throttle_comes_only_from_a_valid_profile() {
        assert!(throttle_from(None).unwrap().is_none(), "off by default");
        assert!(throttle_from(Some("  ")).unwrap().is_none(), "empty is unset");
        let airport = throttle_from(Some(" airport ")).unwrap().unwrap();
        assert_eq!((airport.label.as_str(), airport.throttle), ("airport", axel_proxy::ThrottlePreset::Airport.throttle()));
        assert!(airport.notice().contains("1 Mbit/s up"), "{}", airport.notice());
        assert!(throttle_from(Some("airprot")).is_err(), "a typo is an error, not unthrottled");
        assert!(throttle_from(Some("up=1")).is_err());
    }

    #[test]
    fn the_codex_login_picks_the_upstream() {
        assert_eq!(parse_auth_mode("chatgpt"), OpenAiUpstream::ChatGpt);
        assert_eq!(parse_auth_mode("chatgptAuthTokens"), OpenAiUpstream::ChatGpt);
        assert_eq!(parse_auth_mode("apikey"), OpenAiUpstream::ApiKey);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, r#"{"auth_mode":"chatgpt","tokens":{"access_token":"t"},"OPENAI_API_KEY":null}"#).unwrap();
        assert_eq!(codex_auth_mode(&path).as_deref(), Some("chatgpt"));
        std::fs::write(&path, r#"{"OPENAI_API_KEY":"sk-x"}"#).unwrap();
        assert_eq!(codex_auth_mode(&path), None);
        assert_eq!(codex_auth_mode(&dir.path().join("missing.json")), None);
    }

    #[test]
    fn the_harness_env_points_at_the_proxy_and_the_users_url_becomes_the_upstream() {
        let mut env = vec![("ANTHROPIC_BASE_URL".to_string(), " https://gateway.example ".to_string()), ("PATH".into(), "/bin".into())];
        assert_eq!(user_anthropic_base_url(&env).as_deref(), Some("https://gateway.example"));
        set_env(&mut env, "ANTHROPIC_BASE_URL", "http://127.0.0.1:1/s/anthropic".into());
        assert_eq!(env, [("PATH".to_string(), "/bin".to_string()), ("ANTHROPIC_BASE_URL".to_string(), "http://127.0.0.1:1/s/anthropic".to_string())]);
        assert_eq!(user_anthropic_base_url(&[("ANTHROPIC_BASE_URL".into(), "".into())]), None);
    }

    #[test]
    fn an_inherited_proxy_url_is_told_from_the_users_own() {
        let secret = "ab".repeat(32);
        assert!(is_proxy_url(&format!("http://127.0.0.1:50123/{secret}/anthropic")));
        assert!(is_proxy_url(&format!("http://127.0.0.1:1/{secret}/openai")));
        for own in ["https://api.anthropic.com", "http://127.0.0.1:4000", "http://127.0.0.1:4000/v1/anthropic", &format!("http://localhost:1/{secret}/anthropic"), &format!("http://127.0.0.1:1/{secret}/anthropic/x")] {
            assert!(!is_proxy_url(own), "{own}");
        }
    }

    #[test]
    fn rows_reach_the_store_through_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("axel.db");
        let (sink, writer) = StoreSink::spawn(path.clone()).unwrap();
        let row = ModelRequest {
            id: "R1".into(),
            session_id: "S1".into(),
            thread_id: "T1".into(),
            agent_pubkey: "PK".into(),
            turn: Some(1),
            provider: axel_store::ModelProvider::Anthropic,
            endpoint: "/v1/messages".into(),
            model: None,
            started_at_ms: 1,
            first_byte_ms: None,
            duration_ms: None,
            http_status: None,
            streamed: false,
            outcome: axel_store::RequestOutcome::InFlight,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            request_bytes: 10,
            response_bytes: 0,
            stop_reason: None,
            error: None,
            breakdown: None,
            throttled: true,
        };
        sink.record(&row);
        drop(sink);
        writer.join().unwrap();
        assert_eq!(Store::open(&path).unwrap().model_request("R1").unwrap(), Some(row));
    }

    #[test]
    fn a_store_that_cannot_open_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the database file should be.
        assert!(StoreSink::spawn(dir.path().to_path_buf()).is_err());
    }
}
