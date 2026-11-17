//! The listener and the request path: accept on loopback, refuse what isn't
//! the harness, forward the rest unchanged, stream the answer back.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::request::Parts;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};

use crate::RequestSink;
use crate::analyze::{self, Analysis};
use crate::observe::{Observer, Shape};
use crate::record::{Failure, ObservedBody, Owner, ProxyBody, Recording};
use crate::route::{self, Endpoint, Refusal, Secret};
use crate::upstream::{Client, Upstream, Upstreams};

/// Most connections served at once; one more is closed as it arrives.  A
/// harness keeps a handful open (Claude Code's subagents run in parallel).
pub(crate) const CONNECTIONS_MAX: usize = 32;
/// Request bodies larger than this are refused: Claude Code's largest
/// contexts (a 1M-token window with images) are well under it.
pub(crate) const REQUEST_BODY_BYTES_MAX: usize = 32 * 1024 * 1024;
/// A whole request body must arrive within this long.
const REQUEST_BODY_TIMEOUT_MS: u64 = 60_000;
/// A connection closes when no request head arrives within this long,
/// first request or between two: an idle timeout.
const HEADER_READ_TIMEOUT_MS: u64 = 60_000;
/// Request heads larger than this are refused (hyper's own floor is 8 KiB;
/// Claude Code's heads are about 2 KiB).
const REQUEST_HEAD_BYTES_MAX: usize = 64 * 1024;
const REQUEST_HEADERS_MAX: usize = 128;
/// After an accept error (out of descriptors), wait this long before the next.
const ACCEPT_BACKOFF_MS: u64 = 50;
/// `turn` holds this when the host hasn't said.
pub(crate) const NO_TURN: u64 = u64::MAX;

/// What every connection shares.
pub(crate) struct Shared {
    pub(crate) secret: Secret,
    pub(crate) port: u16,
    pub(crate) upstreams: Upstreams,
    pub(crate) client: Client,
    pub(crate) sink: Arc<dyn RequestSink>,
    pub(crate) session_id: String,
    pub(crate) thread_id: String,
    pub(crate) agent_pubkey: String,
    pub(crate) turn: Arc<AtomicU64>,
    pub(crate) connections: Arc<Semaphore>,
}

impl Shared {
    fn owner(&self) -> Owner {
        let turn = self.turn.load(Ordering::Relaxed);
        Owner {
            session_id: self.session_id.clone(),
            thread_id: self.thread_id.clone(),
            agent_pubkey: self.agent_pubkey.clone(),
            turn: (turn != NO_TURN).then(|| u32::try_from(turn).expect("set_turn stores a u32")),
        }
    }
}

/// Accept until `shutdown` closes.
pub(crate) async fn accept_loop(listener: TcpListener, shared: Arc<Shared>, mut shutdown: watch::Receiver<()>) {
    loop {
        let accepted = tokio::select! {
            _ = shutdown.changed() => return,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(_) => {
                // Out of descriptors or a connection reset before accept:
                // nothing to answer; try again shortly.
                tokio::time::sleep(Duration::from_millis(ACCEPT_BACKOFF_MS)).await;
                continue;
            }
        };
        if !is_allowed_peer(&peer) {
            drop(stream);
            continue;
        }
        let Ok(permit) = shared.connections.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let (shared, shutdown) = (shared.clone(), shutdown.clone());
        tokio::spawn(async move {
            serve_connection(stream, shared, shutdown).await;
            drop(permit);
        });
    }
}

/// Loopback only: the listener binds 127.0.0.1, and this holds even if a
/// future change binds wider.
pub(crate) fn is_allowed_peer(peer: &SocketAddr) -> bool {
    peer.ip().is_loopback()
}

async fn serve_connection(stream: TcpStream, shared: Arc<Shared>, mut shutdown: watch::Receiver<()>) {
    // Streamed tokens should not wait for Nagle.
    if stream.set_nodelay(true).is_err() {
        return;
    }
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(Duration::from_millis(HEADER_READ_TIMEOUT_MS))
        .max_buf_size(REQUEST_HEAD_BYTES_MAX)
        .max_headers(REQUEST_HEADERS_MAX);
    let service = service_fn(move |request| handle(shared.clone(), request));
    let connection = http.serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);
    tokio::select! {
        // A connection error (the harness hung up, a timeout, a bad head) only
        // ends this connection; the request's row says what happened to it.
        _ = connection.as_mut() => {}
        _ = shutdown.changed() => {}
    }
}

async fn handle(shared: Arc<Shared>, request: Request<Incoming>) -> Result<Response<ProxyBody>, Infallible> {
    let started = Instant::now();
    let endpoint = match route::check(request.method(), request.uri(), request.headers(), &shared.secret, shared.port) {
        Ok(endpoint) => endpoint,
        Err(refusal) => return Ok(bare(refusal.status())),
    };
    let (parts, body) = request.into_parts();
    let body = match read_body(&parts.headers, body).await {
        Ok(body) => body,
        Err(refusal) => return Ok(bare(refusal.status())),
    };
    let analysis = analyze_off_thread(endpoint, &parts.headers, &body).await;
    let (upstream, provider) = shared.upstreams.for_side(endpoint.side());
    let request_bytes = body.len() as u64;
    let recording = Recording::start(shared.sink.clone(), shared.owner(), provider, endpoint, analysis, request_bytes, started);
    let outgoing = upstream_request(parts, upstream, endpoint, body);
    match shared.client.request(outgoing).await {
        Ok(response) => Ok(respond(response, endpoint, recording)),
        Err(e) => {
            recording.fail(if e.is_connect() { Failure::Connect } else { Failure::Request });
            Ok(bare(StatusCode::BAD_GATEWAY))
        }
    }
}

/// The whole body, up to [`REQUEST_BODY_BYTES_MAX`] and within
/// [`REQUEST_BODY_TIMEOUT_MS`].  It is read before anything goes upstream,
/// so a refused one never reaches the provider.
async fn read_body(headers: &HeaderMap, body: Incoming) -> Result<Bytes, Refusal> {
    let declared = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > REQUEST_BODY_BYTES_MAX as u64) {
        return Err(Refusal::TooLarge);
    }
    let read = Limited::new(body, REQUEST_BODY_BYTES_MAX).collect();
    let collected = match tokio::time::timeout(Duration::from_millis(REQUEST_BODY_TIMEOUT_MS), read).await {
        Ok(Ok(collected)) => collected,
        Ok(Err(e)) if e.downcast_ref::<LengthLimitError>().is_some() => return Err(Refusal::TooLarge),
        Ok(Err(_)) | Err(_) => return Err(Refusal::BadBody),
    };
    let body = collected.to_bytes();
    assert!(body.len() <= REQUEST_BODY_BYTES_MAX);
    Ok(body)
}

/// The breakdown, worked out on the blocking pool: parsing a large context
/// takes milliseconds, which must not stall other streams.
async fn analyze_off_thread(endpoint: Endpoint, headers: &HeaderMap, body: &Bytes) -> Analysis {
    // A compressed body passes through as it is; it gets sizes, no breakdown.
    if is_encoded(headers) || !endpoint.has_context() {
        return Analysis::default();
    }
    let body = body.clone();
    // A panic while parsing is a bug; the request still goes through.
    tokio::task::spawn_blocking(move || analyze::analyze(endpoint, &body)).await.unwrap_or_default()
}

fn is_encoded(headers: &HeaderMap) -> bool {
    headers.get(header::CONTENT_ENCODING).is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
}

/// The harness's request, for the provider: the same method, headers and
/// body, with the upstream's URL and `Host`, no hop-by-hop headers, and a
/// plain response asked for so its usage can be read.
fn upstream_request(parts: Parts, upstream: &Upstream, endpoint: Endpoint, body: Bytes) -> Request<Full<Bytes>> {
    let uri = upstream.uri(endpoint, parts.uri.query());
    let mut headers = parts.headers;
    strip_hop_by_hop(&mut headers);
    // Hyper sets the length from the body, which is complete.
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(header::HOST, upstream.host());
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    let mut request = Request::new(Full::new(body));
    *request.method_mut() = parts.method;
    *request.uri_mut() = uri;
    *request.headers_mut() = headers;
    request
}

fn respond(response: Response<Incoming>, endpoint: Endpoint, recording: Recording) -> Response<ProxyBody> {
    let (mut parts, body) = response.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    let shape = shape(endpoint, &parts.headers);
    let observer = Observer::new(endpoint.side(), shape);
    let body = ObservedBody::new(body, observer, recording, parts.status);
    Response::from_parts(parts, ProxyBody::Observed(Box::new(body)))
}

/// How to read a response for its usage.
fn shape(endpoint: Endpoint, headers: &HeaderMap) -> Shape {
    if !endpoint.is_inference() || is_encoded(headers) {
        return Shape::Opaque;
    }
    let Some(content_type) = headers.get(header::CONTENT_TYPE) else { return Shape::Sniff };
    let media = content_type.to_str().unwrap_or("").split(';').next().unwrap_or("").trim();
    if media.eq_ignore_ascii_case("text/event-stream") {
        Shape::Stream
    } else if media.eq_ignore_ascii_case("application/json") {
        Shape::Json
    } else {
        Shape::Opaque
    }
}

/// Headers meant for one hop only (RFC 9110 §7.6.1), and any the
/// `Connection` header names.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in ["connection", "keep-alive", "proxy-connection", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade"] {
        headers.remove(name);
    }
}

fn bare(status: StatusCode) -> Response<ProxyBody> {
    let mut response = Response::new(ProxyBody::Empty);
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_peers_are_served() {
        assert!(is_allowed_peer(&"127.0.0.1:5000".parse().unwrap()));
        assert!(is_allowed_peer(&"[::1]:5000".parse().unwrap()));
        assert!(!is_allowed_peer(&"192.168.1.20:5000".parse().unwrap()));
        assert!(!is_allowed_peer(&"100.64.0.1:5000".parse().unwrap()));
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        let mut h = HeaderMap::new();
        for (k, v) in [("connection", "keep-alive, x-private"), ("x-private", "1"), ("keep-alive", "5"), ("transfer-encoding", "chunked"), ("upgrade", "h2c"), ("authorization", "Bearer t")] {
            h.append(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        strip_hop_by_hop(&mut h);
        assert_eq!(h.len(), 1);
        assert!(h.contains_key(header::AUTHORIZATION));
    }
}
