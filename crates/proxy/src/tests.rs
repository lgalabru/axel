//! The proxy end to end, against a fake provider on loopback over plain HTTP
//! (reachable only through the test-only constructor).

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axel_store::{ContextKind, ModelProvider, ModelRequest, RequestOutcome};
use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::upstream::{Upstream, Upstreams};
use crate::{OpenAiUpstream, Proxy, ProxyConfig, RequestSink};

/// Values that must never reach the sink.
const TOKEN: &str = "sk-ant-oat01-TOKEN-NEVER-STORED";
const ACCOUNT: &str = "ACCOUNT-NEVER-STORED";
const PROMPT: &str = "PROMPT-TEXT-NEVER-STORED";
const TOOL_OUTPUT: &str = "TOOL-OUTPUT-NEVER-STORED";
const REPLY_TEXT: &str = "REPLY-TEXT-NEVER-STORED";
/// How long a test waits for something that should take milliseconds.
const WAIT_MS: u64 = 5_000;

#[derive(Default)]
struct Sink {
    rows: Mutex<Vec<ModelRequest>>,
}

impl RequestSink for Sink {
    fn record(&self, request: &ModelRequest) {
        self.rows.lock().unwrap().push(request.clone());
    }
}

impl Sink {
    fn rows(&self) -> Vec<ModelRequest> {
        self.rows.lock().unwrap().clone()
    }

    /// Wait until `n` rows are in.
    async fn wait_for(&self, n: usize) -> Vec<ModelRequest> {
        for _ in 0..WAIT_MS / 10 {
            if self.rows.lock().unwrap().len() >= n {
                return self.rows();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {n} rows, got {:?}", self.rows());
    }
}

/// What the fake provider saw.
#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    path_and_query: String,
    headers: HeaderMap,
    body: Bytes,
}

/// What the fake provider answers: a status, a content type and chunks sent
/// one by one; with `gate`, the chunks after the first wait for a signal.
#[derive(Clone)]
struct Canned {
    status: StatusCode,
    content_type: &'static str,
    chunks: Vec<Bytes>,
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl Canned {
    fn sse(chunks: &[&str]) -> Canned {
        Canned { status: StatusCode::OK, content_type: "text/event-stream", chunks: chunks.iter().map(|c| Bytes::from(c.to_string())).collect(), gate: None }
    }
}

struct Fake {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// A response body fed by a channel, so chunks arrive when the test says.
struct ChannelBody(mpsc::Receiver<Bytes>);

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.0.poll_recv(cx).map(|c| c.map(|b| Ok(Frame::data(b))))
    }
}

async fn fake_provider(base_path: &str, canned: Canned) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}{base_path}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let seen_by_server = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            let (seen, canned) = (seen_by_server.clone(), canned.clone());
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let (seen, canned) = (seen.clone(), canned.clone());
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = body.collect().await.unwrap().to_bytes();
                        let path_and_query = parts.uri.path_and_query().unwrap().to_string();
                        seen.lock().unwrap().push(Seen { method: parts.method, path_and_query, headers: parts.headers, body });
                        let (tx, rx) = mpsc::channel(16);
                        tokio::spawn(async move {
                            for (i, chunk) in canned.chunks.into_iter().enumerate() {
                                if i == 1 && let Some(gate) = &canned.gate {
                                    gate.notified().await;
                                }
                                if tx.send(chunk).await.is_err() {
                                    return;
                                }
                            }
                        });
                        let mut response = Response::builder().status(canned.status);
                        // ChatGPT's Codex backend streams with no content type at all.
                        if !canned.content_type.is_empty() {
                            response = response.header("content-type", canned.content_type);
                        }
                        let response = response.body(ChannelBody(rx)).unwrap();
                        Ok::<_, std::convert::Infallible>(response)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
            });
        }
    });
    Fake { url, seen }
}

fn config() -> ProxyConfig {
    ProxyConfig { session_id: "S1".into(), thread_id: "T1".into(), agent_pubkey: "PK1".into(), anthropic_upstream: None, openai_upstream: OpenAiUpstream::ChatGpt }
}

async fn proxy_to(fake: &Fake, sink: &Arc<Sink>) -> Proxy {
    let upstreams = Upstreams {
        anthropic: Upstream::plain_for_test(&fake.url),
        openai: Upstream::plain_for_test(&fake.url),
        openai_provider: ModelProvider::ChatGpt,
    };
    Proxy::start_for_test(config(), upstreams, sink.clone()).await.unwrap()
}

type Sender = hyper::client::conn::http1::SendRequest<Full<Bytes>>;

async fn connect(port: u16) -> Sender {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(connection);
    sender
}

fn request(proxy: &Proxy, method: Method, path: &str, body: &str) -> Request<Full<Bytes>> {
    Request::builder()
        .method(method)
        .uri(format!("/{}/{path}", proxy.secret()))
        .header("host", format!("127.0.0.1:{}", proxy.port()))
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-api-key", TOKEN)
        .header("chatgpt-account-id", ACCOUNT)
        .header("cookie", format!("session={TOKEN}"))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

async fn send(proxy: &Proxy, request: Request<Full<Bytes>>) -> (StatusCode, Bytes) {
    let mut sender = connect(proxy.port()).await;
    let response = sender.send_request(request).await.unwrap();
    let status = response.status();
    (status, response.into_body().collect().await.unwrap().to_bytes())
}

/// Every row, as text: what the store would hold.
fn assert_nothing_secret(rows: &[ModelRequest]) {
    let text = serde_json::to_string(rows).unwrap() + &format!("{rows:?}");
    for secret in [TOKEN, ACCOUNT, PROMPT, TOOL_OUTPUT, REPLY_TEXT, "Bearer", "session="] {
        assert!(!text.contains(secret), "{secret} reached the sink");
    }
}

fn anthropic_body() -> String {
    serde_json::json!({
        "model": "claude-opus-5-5",
        "stream": true,
        "system": [{"type": "text", "text": "You are Claude Code."}],
        "tools": [{"name": "Bash", "description": "Run a command", "input_schema": {"type": "object"}}],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": PROMPT}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "ls"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu1", "content": TOOL_OUTPUT}]}
        ]
    })
    .to_string()
}

const ANTHROPIC_EVENTS: [&str; 3] = [
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5-5-20261101\",\"usage\":{\"input_tokens\":4,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":26000,\"output_tokens\":1}}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"REPLY-TEXT-NEVER-STORED\"}}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
];

#[tokio::test]
async fn an_anthropic_stream_passes_through_unchanged_and_is_recorded() {
    let fake = fake_provider("", Canned::sse(&ANTHROPIC_EVENTS)).await;
    let sink = Arc::new(Sink::default());
    let proxy = proxy_to(&fake, &sink).await;
    proxy.set_turn(Some(3));
    let body = anthropic_body();
    let (status, received) = send(&proxy, request(&proxy, Method::POST, "anthropic/v1/messages?beta=true", &body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(received, ANTHROPIC_EVENTS.concat().as_bytes(), "byte for byte");

    let seen = fake.seen.lock().unwrap()[0].clone();
    assert_eq!((seen.method, seen.path_and_query.as_str()), (Method::POST, "/v1/messages?beta=true"));
    assert_eq!(seen.body, body.as_bytes(), "the body goes up unchanged");
    assert_eq!(seen.headers["host"], fake.url.trim_start_matches("http://"));
    assert_eq!(seen.headers["accept-encoding"], "identity");
    assert_eq!(seen.headers["authorization"], format!("Bearer {TOKEN}").as_str(), "credentials are forwarded");
    assert_eq!(seen.headers["anthropic-version"], "2023-06-01");

    let rows = sink.wait_for(2).await;
    let (start, end) = (&rows[0], &rows[1]);
    assert_eq!(start.id, end.id);
    assert_eq!(start.outcome, RequestOutcome::InFlight);
    assert_eq!((start.model.as_deref(), start.turn), (Some("claude-opus-5-5"), Some(3)));
    assert_eq!(end.outcome, RequestOutcome::Completed);
    assert_eq!((end.provider, end.endpoint.as_str(), end.http_status, end.streamed), (ModelProvider::Anthropic, "/v1/messages", Some(200), true));
    assert_eq!(end.model.as_deref(), Some("claude-opus-5-5-20261101"));
    assert_eq!((end.input_tokens, end.cache_write_tokens, end.cache_read_tokens, end.output_tokens), (Some(4), Some(100), Some(26_000), Some(7)));
    assert_eq!(end.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!((end.request_bytes, end.response_bytes), (body.len() as u64, ANTHROPIC_EVENTS.concat().len() as u64));
    assert!(end.first_byte_ms.is_some() && end.duration_ms.is_some() && end.error.is_none());
    let breakdown = end.breakdown.as_ref().unwrap();
    assert_eq!((breakdown.messages, breakdown.tools), (3, 1));
    for kind in [ContextKind::System, ContextKind::ToolDefinition, ContextKind::UserText, ContextKind::ToolCall, ContextKind::ToolResult] {
        assert!(breakdown.parts.iter().any(|p| p.kind == kind), "{kind:?}");
    }
    assert!(breakdown.parts.iter().any(|p| p.kind == ContextKind::ToolResult && p.label == "Bash"));
    assert_nothing_secret(&rows);
}

const OPENAI_EVENTS: [&str; 2] = [
    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5.5-codex\",\"status\":\"in_progress\"}}\n\n",
    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"REPLY-TEXT-NEVER-STORED\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.5-codex\",\"status\":\"completed\",\"usage\":{\"input_tokens\":9000,\"input_tokens_details\":{\"cached_tokens\":8000},\"output_tokens\":40,\"output_tokens_details\":{\"reasoning_tokens\":16}}}}\n\n",
];

#[tokio::test]
async fn an_openai_stream_passes_through_unchanged_and_is_recorded() {
    let fake = fake_provider("/backend-api/codex", Canned::sse(&OPENAI_EVENTS)).await;
    let sink = Arc::new(Sink::default());
    let proxy = proxy_to(&fake, &sink).await;
    let body = serde_json::json!({
        "model": "gpt-5.5-codex",
        "instructions": "You are Codex.",
        "tools": [{"type": "function", "name": "shell", "parameters": {}}],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": PROMPT}]},
            {"type": "function_call", "name": "shell", "call_id": "c1", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": TOOL_OUTPUT}
        ],
        "stream": true
    })
    .to_string();
    let (status, received) = send(&proxy, request(&proxy, Method::POST, "openai/responses", &body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(received, OPENAI_EVENTS.concat().as_bytes());
    let (models_status, _) = send(&proxy, request(&proxy, Method::GET, "openai/models?client_version=0.99.0", "")).await;
    assert_eq!(models_status, StatusCode::OK);

    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(seen[0].path_and_query, "/backend-api/codex/responses");
    assert_eq!(seen[0].headers["chatgpt-account-id"], ACCOUNT);
    assert_eq!(seen[1].path_and_query, "/backend-api/codex/models?client_version=0.99.0");
    let rows = sink.wait_for(4).await;
    let end = rows.iter().find(|r| r.endpoint == "/responses" && r.outcome != RequestOutcome::InFlight).unwrap();
    assert_eq!(end.provider, ModelProvider::ChatGpt);
    assert_eq!(end.turn, None);
    assert_eq!((end.input_tokens, end.cache_read_tokens, end.output_tokens, end.reasoning_tokens), (Some(1000), Some(8000), Some(40), Some(16)));
    assert_eq!(end.stop_reason.as_deref(), Some("completed"));
    let breakdown = end.breakdown.as_ref().unwrap();
    assert!(breakdown.parts.iter().any(|p| p.kind == ContextKind::ToolResult && p.label == "shell"));
    let models = rows.iter().find(|r| r.endpoint == "/models" && r.outcome == RequestOutcome::Completed).unwrap();
    assert!(models.breakdown.is_none() && models.input_tokens.is_none());
    assert_nothing_secret(&rows);
}

#[tokio::test]
async fn the_response_streams_as_it_arrives() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let canned = Canned { gate: Some(gate.clone()), ..Canned::sse(&ANTHROPIC_EVENTS) };
    let fake = fake_provider("", canned).await;
    let sink = Arc::new(Sink::default());
    let proxy = proxy_to(&fake, &sink).await;
    let mut sender = connect(proxy.port()).await;
    let response = sender.send_request(request(&proxy, Method::POST, "anthropic/v1/messages", &anthropic_body())).await.unwrap();
    let mut body = response.into_body();
    // The provider holds the rest back until the first event has reached us.
    let first = tokio::time::timeout(Duration::from_millis(WAIT_MS), body.frame()).await.expect("the first event arrives alone").unwrap().unwrap();
    assert_eq!(first.into_data().unwrap(), ANTHROPIC_EVENTS[0].as_bytes());
    gate.notify_one();
    let rest = body.collect().await.unwrap().to_bytes();
    assert_eq!(rest, ANTHROPIC_EVENTS[1..].concat().as_bytes());
}

#[tokio::test]
async fn a_harness_that_hangs_up_mid_stream_is_recorded_as_cancelled() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let canned = Canned { gate: Some(gate), ..Canned::sse(&ANTHROPIC_EVENTS) };
    let fake = fake_provider("", canned).await;
    let sink = Arc::new(Sink::default());
    let proxy = proxy_to(&fake, &sink).await;
    let mut sender = connect(proxy.port()).await;
    let response = sender.send_request(request(&proxy, Method::POST, "anthropic/v1/messages", &anthropic_body())).await.unwrap();
    let mut body = response.into_body();
    body.frame().await.unwrap().unwrap();
    drop(body);
    drop(sender);
    let rows = sink.wait_for(2).await;
    assert_eq!(rows[1].outcome, RequestOutcome::Cancelled);
    assert_eq!(rows[1].input_tokens, Some(4), "what was seen before the hang-up is kept");
}

#[tokio::test]
async fn a_stream_without_a_content_type_that_the_harness_leaves_after_its_last_event_is_complete() {
    // Codex stops reading at `response.completed`, before the body's end.
    let gate = Arc::new(tokio::sync::Notify::new());
    let canned = Canned { content_type: "", gate: Some(gate), ..Canned::sse(&[&OPENAI_EVENTS.concat(), "\n"]) };
    let fake = fake_provider("", canned).await;
    let sink = Arc::new(Sink::default());
    let proxy = proxy_to(&fake, &sink).await;
    let mut sender = connect(proxy.port()).await;
    let response = sender.send_request(request(&proxy, Method::POST, "openai/responses", "{\"model\":\"gpt-5.5-codex\"}")).await.unwrap();
    let mut body = response.into_body();
    body.frame().await.unwrap().unwrap();
    drop(body);
    drop(sender);
    let rows = sink.wait_for(2).await;
    assert_eq!((rows[1].outcome, rows[1].streamed, rows[1].output_tokens), (RequestOutcome::Completed, true, Some(40)));
    assert_eq!(rows[1].error, None);
}

#[tokio::test]
async fn provider_errors_and_unreachable_providers_are_recorded() {
    let canned = Canned { status: StatusCode::TOO_MANY_REQUESTS, content_type: "application/json", chunks: vec![Bytes::from_static(br#"{"type":"error","error":{"type":"rate_limit_error","message":"PROMPT-TEXT-NEVER-STORED"}}"#)], gate: None };
    let fake = fake_provider("", canned).await;
    let sink = Arc::new(Sink::default());
    let proxy = proxy_to(&fake, &sink).await;
    let (status, _) = send(&proxy, request(&proxy, Method::POST, "anthropic/v1/messages", &anthropic_body())).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let rows = sink.wait_for(2).await;
    assert_eq!(rows[1].outcome, RequestOutcome::HttpError);
    assert_eq!(rows[1].error.as_deref(), Some("the provider answered 429 (rate_limit_error)"));
    assert_nothing_secret(&rows);

    // A port nothing listens on.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = Fake { url: format!("http://{}", closed.local_addr().unwrap()), seen: Arc::default() };
    drop(closed);
    let sink = Arc::new(Sink::default());
    let proxy = proxy_to(&dead, &sink).await;
    let (status, _) = send(&proxy, request(&proxy, Method::POST, "anthropic/v1/messages", &anthropic_body())).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let rows = sink.wait_for(2).await;
    assert_eq!((rows[1].outcome, rows[1].error.as_deref()), (RequestOutcome::Failed, Some("could not connect to the provider")));
}

/// Send raw bytes and read the status line of the answer (or nothing, if
/// the proxy closed the connection).
async fn raw(port: u16, what: &str, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut answer = Vec::new();
    let read = tokio::time::timeout(Duration::from_millis(WAIT_MS), stream.read_to_end(&mut answer)).await;
    assert!(read.is_ok(), "{what}: the proxy answers and closes");
    String::from_utf8_lossy(&answer).lines().next().unwrap_or("").to_string()
}

#[tokio::test]
async fn requests_the_proxy_refuses_never_reach_the_provider() {
    let fake = fake_provider("", Canned::sse(&ANTHROPIC_EVENTS)).await;
    let sink = Arc::new(Sink::default());
    let proxy = proxy_to(&fake, &sink).await;
    let (port, secret) = (proxy.port(), proxy.secret().to_string());
    let host = format!("127.0.0.1:{port}");
    let post = |path: &str, extra: &str| format!("POST {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Content-Length: 2\r\nConnection: close\r\n\r\n{{}}");
    let wrong_secret = "0".repeat(64);
    let cases: Vec<(&str, String, &str)> = vec![
        ("wrong secret", post(&format!("/{wrong_secret}/anthropic/v1/messages"), ""), "404"),
        ("no secret", post("/anthropic/v1/messages", ""), "404"),
        ("secret prefix only", post(&format!("/{}/anthropic/v1/messages", &secret[..32]), ""), "404"),
        ("outside the allowlist", post(&format!("/{secret}/anthropic/v1/complete"), ""), "404"),
        ("path traversal", post(&format!("/{secret}/anthropic/v1/messages/../../admin"), ""), "404"),
        ("other side's path", post(&format!("/{secret}/openai/v1/messages"), ""), "404"),
        ("wrong method", format!("DELETE /{secret}/anthropic/v1/messages HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"), "405"),
        ("Origin", post(&format!("/{secret}/anthropic/v1/messages"), "Origin: https://evil.example\r\n"), "403"),
        ("Origin null", post(&format!("/{secret}/anthropic/v1/messages"), "Origin: null\r\n"), "403"),
        ("foreign Host", post(&format!("/{secret}/anthropic/v1/messages"), "").replace(&host, "evil.example"), "403"),
        ("rebinding Host", post(&format!("/{secret}/anthropic/v1/messages"), "").replace(&host, &format!("evil.example:{port}")), "403"),
        ("absolute URI", post(&format!("http://api.anthropic.com/{secret}/anthropic/v1/messages"), ""), "400"),
        ("CONNECT", "CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: api.anthropic.com:443\r\nConnection: close\r\n\r\n".to_string(), "405"),
        (
            "oversized body",
            format!("POST /{secret}/anthropic/v1/messages HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", 32 * 1024 * 1024 + 1),
            "413",
        ),
    ];
    for (what, request, status) in cases {
        let line = raw(port, what, request.as_bytes()).await;
        assert!(line.starts_with(&format!("HTTP/1.1 {status}")), "{what}: {line:?}");
    }
    assert!(fake.seen.lock().unwrap().is_empty(), "no refused request reached the provider");
    assert!(sink.rows().is_empty(), "refusals are not recorded");
}

#[tokio::test]
async fn a_chunked_body_past_the_limit_is_refused_while_it_streams() {
    let fake = fake_provider("", Canned::sse(&ANTHROPIC_EVENTS)).await;
    let sink = Arc::new(Sink::default());
    let proxy = proxy_to(&fake, &sink).await;
    let mut stream = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
    let head = format!("POST /{}/anthropic/v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nTransfer-Encoding: chunked\r\n\r\n", proxy.secret(), proxy.port());
    stream.write_all(head.as_bytes()).await.unwrap();
    let chunk = vec![b'a'; 1024 * 1024];
    let mut chunk_frame = format!("{:x}\r\n", chunk.len()).into_bytes();
    chunk_frame.extend_from_slice(&chunk);
    chunk_frame.extend_from_slice(b"\r\n");
    for _ in 0..33 {
        // The proxy may close as soon as it has seen enough.
        if stream.write_all(&chunk_frame).await.is_err() {
            break;
        }
    }
    let mut answer = vec![0u8; 64];
    let n = tokio::time::timeout(Duration::from_millis(WAIT_MS), stream.read(&mut answer)).await.unwrap().unwrap_or(0);
    assert!(String::from_utf8_lossy(&answer[..n]).starts_with("HTTP/1.1 413"));
    assert!(fake.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn connections_past_the_limit_are_closed() {
    let fake = fake_provider("", Canned::sse(&ANTHROPIC_EVENTS)).await;
    let sink = Arc::new(Sink::default());
    let proxy = proxy_to(&fake, &sink).await;
    let mut open = Vec::new();
    for _ in 0..crate::server::CONNECTIONS_MAX {
        open.push(TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap());
    }
    let mut one_more = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_millis(WAIT_MS), one_more.read(&mut byte)).await.expect("closed, not left waiting");
    assert!(matches!(read, Ok(0) | Err(_)), "the connection past the limit is closed");
    // A slot freed is a slot served.
    drop(open.pop());
    for attempt in 0.. {
        let mut stream = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        let get = format!("GET /{}/anthropic/v1/models HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", proxy.secret(), proxy.port());
        stream.write_all(get.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        let _ = tokio::time::timeout(Duration::from_millis(WAIT_MS), stream.read_to_end(&mut answer)).await;
        if answer.starts_with(b"HTTP/1.1 200") {
            break;
        }
        assert!(attempt < 100, "a freed slot is served again");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_plain_http_upstream_is_refused_at_start() {
    let sink: Arc<dyn RequestSink> = Arc::new(Sink::default());
    let mut config = config();
    config.anthropic_upstream = Some("http://127.0.0.1:8080".into());
    assert!(Proxy::start(config.clone(), sink.clone()).await.is_err());
    config.anthropic_upstream = Some("https://gateway.example/anthropic".into());
    let proxy = Proxy::start(config, sink).await.unwrap();
    let url = proxy.anthropic_base_url();
    assert!(url.starts_with("http://127.0.0.1:") && url.ends_with("/anthropic"));
    assert!(proxy.openai_base_url().ends_with("/openai"));
    proxy.shutdown().await;
}

#[tokio::test]
async fn shutdown_stops_the_listener() {
    let fake = fake_provider("", Canned::sse(&ANTHROPIC_EVENTS)).await;
    let sink = Arc::new(Sink::default());
    let proxy = proxy_to(&fake, &sink).await;
    let port = proxy.port();
    proxy.shutdown().await;
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
}
