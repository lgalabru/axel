//! The web pane's server: HTTP/1.1 on `127.0.0.1`, one thread, one owner.
//!
//! Only the page the pane opened may talk to it.  The first request carries
//! a secret made at start (`/?k=…`); the reply sets it as a cookie and drops
//! it from the address.  Every other request needs that cookie, the right
//! `Host` (so a page elsewhere can't reach us through DNS rebinding), and,
//! when it changes something, a JSON body from our own origin.  There is no
//! framework: a few routes, every read and queue bounded, one connection
//! per request except the event stream.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// The most bytes of request line and headers read: a browser's request is
/// well under 4 KiB; past this it is not a browser.
const HEADER_BYTES_MAX: usize = 16 * 1024;
/// The most bytes of request body: a typed message, never a file (files
/// stay with the terminal composer for now).
pub const BODY_BYTES_MAX: usize = 256 * 1024;
/// The most connections served at once: one page makes two or three.
const CONNECTIONS_MAX: usize = 32;
/// The most event streams at once: one per open page, and a page or two
/// reloading.
const STREAMS_MAX: usize = 8;
/// How long a slow client gets to send its request.
const READ_TIMEOUT_SECS: u64 = 10;
/// How often the thread is looked at for changes.  Fast enough that a
/// streaming reply reads as live, cheap enough to do all day (`RFD 0007`).
const POLL_MS: u64 = 300;
/// Longest the view goes without a full reload even when the change
/// marker stands still: what the marker can't see (a session host that
/// died) shows within this.
const RELOAD_SECS_MAX: u64 = 5;
/// How long an event stream stays quiet before it sends a ping, so a dead
/// page is noticed and its stream freed.
const PING_SECS: u64 = 15;

/// What the page shows, as of `version`.
struct Snapshot {
    version: u64,
    json: Arc<String>,
}

/// State the server's threads share.
struct Shared {
    snapshot: Mutex<Snapshot>,
    changed: Condvar,
    connections: AtomicUsize,
    streams: AtomicUsize,
    /// Requests served, for tests and the log.
    served: AtomicU64,
}

/// What the routes act on, given to the server by its owner.
pub trait Backend: Send + Sync + 'static {
    /// The thread as JSON ([`super::view::thread`]).
    fn load(&self) -> Result<Value>;
    /// A value that changes whenever [`Backend::load`] might return
    /// something new; much cheaper than loading.  `None` when it can't tell.
    fn marker(&self) -> Option<(i64, i64)>;
    /// Do `action` (`send`, `answer`, `cancel`) with the page's JSON body.
    fn act(&self, action: Action, body: &Value) -> Result<Value>;
}

/// What a POST asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Send,
    Answer,
    Cancel,
    /// Carry out what the thread controls page picked.
    Control,
    /// Open the thread controls (Ctrl+D in the composer).
    OpenControls,
}

impl Action {
    fn from_path(path: &str) -> Option<Action> {
        match path {
            "/api/send" => Some(Action::Send),
            "/api/answer" => Some(Action::Answer),
            "/api/cancel" => Some(Action::Cancel),
            "/api/control" => Some(Action::Control),
            "/api/open_controls" => Some(Action::OpenControls),
            _ => None,
        }
    }
}

/// A running server.
pub struct Running {
    pub port: u16,
    secret: String,
    shared: Arc<Shared>,
    rendered: Arc<Rendered>,
}

impl Running {
    /// The address the pane opens: it carries the secret, once.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/?k={}", self.port, self.secret)
    }

    /// Requests served so far.
    pub fn served(&self) -> u64 {
        self.shared.served.load(Ordering::Relaxed)
    }

    /// Serve the page with `theme` from the next load on (`theme::css`):
    /// Ladybird reloads it when it starts again at another scale.
    pub fn set_theme(&self, theme: &str) {
        let css = stylesheet(theme);
        // A poisoned lock means a request thread panicked while reading:
        // the stylesheet itself is whole, so it is replaced all the same.
        *self.rendered.css.write().unwrap_or_else(std::sync::PoisonError::into_inner) = css;
    }
}

/// The page's stylesheet with `theme`'s properties on its root, after its
/// own: in the stylesheet, as the page's CSP allows no inline style.
fn stylesheet(theme: &str) -> String {
    assert!(!theme.contains(['{', '}', '<']), "a theme is CSS properties only");
    if theme.is_empty() {
        super::assets::CSS.to_string()
    } else {
        format!("{}\n:root {{ {theme} }}\n", super::assets::CSS)
    }
}

/// Bind a free port on `127.0.0.1` and serve `backend` from background threads.
/// `theme` is CSS custom properties (`theme::css`), set on the page's root
/// after its own: in the stylesheet, as the page's CSP allows no inline style.
pub fn start(backend: Arc<dyn Backend>, view: super::WebView, theme: &str) -> Result<Running> {
    let listener = TcpListener::bind("127.0.0.1:0").context("listening on 127.0.0.1")?;
    let port = listener.local_addr()?.port();
    assert!(port > 0, "the OS gave a real port");
    let secret = axel_identity::Keypair::generate().public().to_base58();
    let first = backend.load().unwrap_or_else(|e| json!({ "error": format!("{e:#}") }));
    let shared = Arc::new(Shared {
        snapshot: Mutex::new(Snapshot { version: 1, json: Arc::new(first.to_string()) }),
        changed: Condvar::new(),
        connections: AtomicUsize::new(0),
        streams: AtomicUsize::new(0),
        served: AtomicU64::new(0),
    });
    let poller = (shared.clone(), backend.clone());
    std::thread::spawn(move || poll(&poller.0, poller.1.as_ref()));
    let page = super::assets::PAGE.replace("{{view}}", view.as_str());
    assert!(page.contains(&format!("data-view=\"{}\"", view.as_str())), "the page names its view");
    let rendered = Arc::new(Rendered { page, css: std::sync::RwLock::new(stylesheet(theme)) });
    let accept = (shared.clone(), backend, secret.clone(), rendered.clone());
    std::thread::spawn(move || serve(listener, port, &accept.0, &accept.1, &accept.2, &accept.3));
    Ok(Running { port, secret, shared, rendered })
}

/// Look at the thread every [`POLL_MS`] and wake the event streams when
/// what the page shows changed.
///
/// A full load costs about 8 ms on a 10 000-item thread (RFD 0007), so it
/// runs only when the backend's change marker moved, or every
/// [`RELOAD_SECS_MAX`]; a still thread costs two single-row reads a poll.
fn poll(shared: &Shared, backend: &dyn Backend) {
    let mut last_marker = backend.marker();
    let mut last_load = std::time::Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(POLL_MS));
        let marker = backend.marker();
        let stale = last_load.elapsed() >= Duration::from_secs(RELOAD_SECS_MAX);
        if marker.is_some() && marker == last_marker && !stale {
            continue;
        }
        last_marker = marker;
        last_load = std::time::Instant::now();
        // A failed load keeps what the page has; the next poll tries again.
        let Ok(view) = backend.load() else { continue };
        let json = view.to_string();
        let Ok(mut snapshot) = shared.snapshot.lock() else { return };
        if *snapshot.json != json {
            snapshot.version += 1;
            snapshot.json = Arc::new(json);
            shared.changed.notify_all();
        }
    }
}

/// The page and its stylesheet, as this pane serves them.
struct Rendered {
    page: String,
    css: std::sync::RwLock<String>,
}

fn serve(listener: TcpListener, port: u16, shared: &Arc<Shared>, backend: &Arc<dyn Backend>, secret: &str, page: &Arc<Rendered>) {
    for stream in listener.incoming() {
        // A connection that failed before it was accepted is the client's loss.
        let Ok(mut stream) = stream else { continue };
        if shared.connections.load(Ordering::Acquire) >= CONNECTIONS_MAX {
            // Refused politely; nothing more to do if even that fails.
            let _ = respond(&mut stream, 503, "text/plain", b"busy");
            continue;
        }
        shared.connections.fetch_add(1, Ordering::AcqRel);
        let (shared, backend, secret, page) = (shared.clone(), backend.clone(), secret.to_string(), page.clone());
        std::thread::spawn(move || {
            // A connection that breaks mid-request has no one left to tell.
            let _ = handle(&mut stream, port, &shared, backend.as_ref(), &secret, &page);
            shared.served.fetch_add(1, Ordering::Relaxed);
            shared.connections.fetch_sub(1, Ordering::AcqRel);
        });
    }
}

/// One parsed request.
#[derive(Debug, Default)]
struct Request {
    method: String,
    path: String,
    query: String,
    host: Option<String>,
    cookie: Option<String>,
    origin: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// Why a request was refused before any route saw it.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    BadRequest,
    TooLarge,
}

fn read_request(stream: &TcpStream) -> std::result::Result<Request, Refusal> {
    stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS))).map_err(|_| Refusal::BadRequest)?;
    let mut reader = BufReader::new(stream.take((HEADER_BYTES_MAX + BODY_BYTES_MAX) as u64));
    let mut header_bytes = 0_usize;
    let mut line = String::new();
    let mut request = Request::default();
    let mut content_length = 0_usize;
    let mut first = true;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|_| Refusal::BadRequest)?;
        header_bytes += n;
        if n == 0 || header_bytes > HEADER_BYTES_MAX {
            return Err(if n == 0 { Refusal::BadRequest } else { Refusal::TooLarge });
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if first {
            parse_request_line(trimmed, &mut request)?;
            first = false;
            continue;
        }
        if trimmed.is_empty() {
            break;
        }
        let Some((name, value)) = trimmed.split_once(':') else { return Err(Refusal::BadRequest) };
        let value = value.trim().to_string();
        match name.trim().to_ascii_lowercase().as_str() {
            "host" => request.host = Some(value),
            "cookie" => request.cookie = Some(value),
            "origin" => request.origin = Some(value),
            "content-type" => request.content_type = Some(value),
            "content-length" => content_length = value.parse().map_err(|_| Refusal::BadRequest)?,
            _ => {}
        }
    }
    if content_length > BODY_BYTES_MAX {
        return Err(Refusal::TooLarge);
    }
    request.body = vec![0; content_length];
    reader.read_exact(&mut request.body).map_err(|_| Refusal::BadRequest)?;
    Ok(request)
}

fn parse_request_line(line: &str, request: &mut Request) -> std::result::Result<(), Refusal> {
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(Refusal::BadRequest);
    };
    if !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err(Refusal::BadRequest);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    request.method = method.to_string();
    request.path = path.to_string();
    request.query = query.to_string();
    Ok(())
}

/// The cookie that proves a request comes from the pane's page.
const COOKIE: &str = "axel_web";

fn has_cookie(request: &Request, secret: &str) -> bool {
    let Some(header) = &request.cookie else { return false };
    header.split(';').filter_map(|pair| pair.trim().split_once('=')).any(|(name, value)| name == COOKIE && same(value, secret))
}

/// Compares in time independent of where the first difference is.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').filter_map(|pair| pair.split_once('=')).find_map(|(k, v)| (k == key).then_some(v))
}

fn handle(stream: &mut TcpStream, port: u16, shared: &Shared, backend: &dyn Backend, secret: &str, page: &Rendered) -> std::io::Result<()> {
    let request = match read_request(stream) {
        Ok(r) => r,
        Err(Refusal::TooLarge) => return respond(stream, 413, "text/plain", b"too large"),
        Err(Refusal::BadRequest) => return respond(stream, 400, "text/plain", b"bad request"),
    };
    let own_host = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if !request.host.as_ref().is_some_and(|h| own_host.contains(h)) {
        return respond(stream, 403, "text/plain", b"wrong host");
    }
    // The pane's first request: trade the secret in the address for a cookie.
    if request.method == "GET" && request.path == "/" && query_value(&request.query, "k").is_some_and(|k| same(k, secret)) {
        let headers = format!(
            "HTTP/1.1 303 See Other\r\nLocation: /\r\nSet-Cookie: {COOKIE}={secret}; HttpOnly; SameSite=Strict; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return stream.write_all(headers.as_bytes());
    }
    if !has_cookie(&request, secret) {
        return respond(stream, 403, "text/plain", b"this page belongs to an Axel pane");
    }
    route(stream, &request, port, shared, backend, page)
}

fn route(stream: &mut TcpStream, request: &Request, port: u16, shared: &Shared, backend: &dyn Backend, page: &Rendered) -> std::io::Result<()> {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => respond(stream, 200, "text/html; charset=utf-8", page.page.as_bytes()),
        ("GET", "/app.css") => {
            let css = page.css.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            respond(stream, 200, "text/css; charset=utf-8", css.as_bytes())
        }
        ("GET", path) if path.starts_with("/fonts/") => match super::assets::FONTS.iter().find(|(p, _)| *p == path) {
            Some((_, bytes)) => respond(stream, 200, "font/woff2", bytes),
            None => respond(stream, 404, "text/plain", b"no such font"),
        },
        ("GET", "/app.js") => respond(stream, 200, "text/javascript; charset=utf-8", super::assets::JS.as_bytes()),
        ("GET", "/api/thread") => {
            let json = shared.snapshot.lock().map(|s| s.json.clone()).unwrap_or_default();
            respond(stream, 200, "application/json", json.as_bytes())
        }
        ("GET", "/api/events") => events(stream, shared),
        ("POST", path) => match Action::from_path(path) {
            Some(action) => act(stream, request, port, backend, action),
            None => respond(stream, 404, "text/plain", b"not found"),
        },
        _ => respond(stream, 404, "text/plain", b"not found"),
    }
}

fn act(stream: &mut TcpStream, request: &Request, port: u16, backend: &dyn Backend, action: Action) -> std::io::Result<()> {
    // A form or an image elsewhere can't send JSON from our origin: this is
    // what keeps other pages from acting through the browser's cookie.
    let json_body = request.content_type.as_deref().is_some_and(|c| c.starts_with("application/json"));
    let own_origin = request.origin.as_deref().is_none_or(|o| o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}"));
    if !json_body || !own_origin {
        return respond(stream, 403, "text/plain", b"not from this page");
    }
    let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
        return respond(stream, 400, "application/json", br#"{"error":"the body isn't JSON"}"#);
    };
    match backend.act(action, &body) {
        Ok(reply) => respond(stream, 200, "application/json", reply.to_string().as_bytes()),
        Err(e) => respond(stream, 409, "application/json", json!({ "error": format!("{e:#}") }).to_string().as_bytes()),
    }
}

/// Server-sent events: one `data: VERSION` line each time the thread changes,
/// a ping when nothing does, until the page goes away.
fn events(stream: &mut TcpStream, shared: &Shared) -> std::io::Result<()> {
    if shared.streams.fetch_add(1, Ordering::AcqRel) >= STREAMS_MAX {
        shared.streams.fetch_sub(1, Ordering::AcqRel);
        return respond(stream, 503, "text/plain", b"too many pages");
    }
    let result = stream_events(stream, shared);
    shared.streams.fetch_sub(1, Ordering::AcqRel);
    result
}

fn stream_events(stream: &mut TcpStream, shared: &Shared) -> std::io::Result<()> {
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\nretry: 1000\n\n")?;
    let mut seen = 0_u64;
    loop {
        let version = {
            let Ok(guard) = shared.snapshot.lock() else { return Ok(()) };
            let (guard, _) = shared
                .changed
                .wait_timeout_while(guard, Duration::from_secs(PING_SECS), |s| s.version == seen)
                .map_err(|_| std::io::Error::other("poisoned"))?;
            guard.version
        };
        if version == seen {
            stream.write_all(b": ping\n\n")?;
        } else {
            seen = version;
            stream.write_all(format!("data: {version}\n\n").as_bytes())?;
        }
        stream.flush()?;
    }
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        other => unreachable!("status {other} is not one this server sends"),
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; style-src 'self'; script-src 'self'; img-src 'self' data:\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        view: Mutex<Value>,
        acted: Mutex<Vec<(Action, Value)>>,
    }

    impl Backend for Fake {
        fn load(&self) -> Result<Value> {
            Ok(self.view.lock().unwrap().clone())
        }
        fn marker(&self) -> Option<(i64, i64)> {
            // The fake can't tell, so every poll loads.
            None
        }
        fn act(&self, action: Action, body: &Value) -> Result<Value> {
            self.acted.lock().unwrap().push((action, body.clone()));
            if body["text"] == "fail" { anyhow::bail!("no agent here") } else { Ok(json!({ "ok": true })) }
        }
    }

    fn fake() -> Arc<Fake> {
        Arc::new(Fake { view: Mutex::new(json!({ "title": "t" })), acted: Mutex::new(vec![]) })
    }

    /// Send raw `request` and return the status code and the whole reply.
    fn send(port: u16, request: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        // A font is no UTF-8: the reply is read as bytes, its text kept.
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        let reply = String::from_utf8_lossy(&bytes).into_owned();
        let status = reply.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        (status, reply)
    }

    fn get(port: u16, path: &str, cookie: Option<&str>) -> (u16, String) {
        let cookie = cookie.map(|c| format!("Cookie: {COOKIE}={c}\r\n")).unwrap_or_default();
        send(port, &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}\r\n"))
    }

    fn post(port: u16, path: &str, cookie: &str, extra: &str, body: &str) -> (u16, String) {
        send(port, &format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {COOKIE}={cookie}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\n\r\n{body}",
            body.len()
        ))
    }

    #[test]
    fn the_secret_becomes_a_cookie_and_nothing_works_without_it() {
        let backend = fake();
        let server = start(backend.clone(), super::super::WebView::Messages, "").unwrap();
        let (port, secret) = (server.port, server.secret.clone());
        let (status, reply) = get(port, &format!("/?k={secret}"), None);
        assert_eq!(status, 303);
        assert!(reply.contains(&format!("Set-Cookie: {COOKIE}={secret}; HttpOnly; SameSite=Strict")), "{reply}");
        assert_eq!(get(port, "/", None).0, 403);
        assert_eq!(get(port, "/?k=wrong", None).0, 403);
        assert_eq!(get(port, "/api/thread", Some("wrong")).0, 403);
        let (status, page) = get(port, "/", Some(&secret));
        assert_eq!(status, 200);
        assert!(page.contains("<html"), "the page");
        let (status, view) = get(port, "/api/thread", Some(&secret));
        assert_eq!(status, 200);
        assert!(view.ends_with(r#"{"title":"t"}"#), "{view}");
    }

    #[test]
    fn the_stylesheet_ends_with_the_panes_colours() {
        let server = start(fake(), super::super::WebView::Messages, "--bg:#fafaf0;").unwrap();
        let (status, css) = get(server.port, "/app.css", Some(&server.secret));
        assert_eq!(status, 200);
        assert!(css.trim_end().ends_with(":root { --bg:#fafaf0; }"), "the theme comes after the page's own palette");
        let plain = start(fake(), super::super::WebView::Messages, "").unwrap();
        assert!(!get(plain.port, "/app.css", Some(&plain.secret)).1.contains(":root { --bg:#fafaf0"));
    }

    #[test]
    fn the_theme_follows_a_restart_and_the_fonts_are_served() {
        let server = start(fake(), super::super::WebView::Messages, "").unwrap();
        server.set_theme("--font-size:21.00px;");
        let (_, css) = get(server.port, "/app.css", Some(&server.secret));
        assert!(css.trim_end().ends_with(":root { --font-size:21.00px; }"), "the next load has the new theme");
        let (status, font) = get(server.port, "/fonts/JetBrainsMono-Regular.woff2", Some(&server.secret));
        assert_eq!(status, 200);
        assert!(font.contains("font/woff2"), "served as a font");
        assert_eq!(get(server.port, "/fonts/nope.woff2", Some(&server.secret)).0, 404);
        assert_eq!(get(server.port, "/fonts/JetBrainsMono-Regular.woff2", None).0, 403, "behind the cookie like the rest");
    }

    #[test]
    fn nothing_on_the_page_animates() {
        // Ladybird sends the whole pane for any change: an animation keeps it
        // drawing 60 full frames a second, and scrolling lags behind them.
        assert!(!super::super::assets::CSS.contains("@keyframes"), "no CSS animation");
        assert!(!super::super::assets::JS.contains("setInterval"), "no ticking script");
    }

    #[test]
    fn a_page_on_another_host_or_origin_is_refused() {
        let server = start(fake(), super::super::WebView::Messages, "").unwrap();
        let (port, secret) = (server.port, server.secret.clone());
        // DNS rebinding: our port, someone else's name.
        let (status, _) = send(port, &format!("GET /api/thread HTTP/1.1\r\nHost: evil.example:{port}\r\nCookie: {COOKIE}={secret}\r\n\r\n"));
        assert_eq!(status, 403);
        let (status, _) = post(port, "/api/send", &secret, "Origin: http://evil.example\r\n", r#"{"text":"hi"}"#);
        assert_eq!(status, 403);
        let form = format!(
            "POST /api/send HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {COOKIE}={secret}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 4\r\n\r\ntext"
        );
        assert_eq!(send(port, &form).0, 403, "a form post");
    }

    #[test]
    fn actions_reach_the_backend_and_its_errors_reach_the_page() {
        let backend = fake();
        let server = start(backend.clone(), super::super::WebView::Messages, "").unwrap();
        let (port, secret) = (server.port, server.secret.clone());
        let (status, _) = post(port, "/api/send", &secret, &format!("Origin: http://127.0.0.1:{port}\r\n"), r#"{"text":"hi","steer":false}"#);
        assert_eq!(status, 200);
        let (status, reply) = post(port, "/api/send", &secret, "", r#"{"text":"fail"}"#);
        assert_eq!(status, 409);
        assert!(reply.contains("no agent here"), "{reply}");
        assert_eq!(post(port, "/api/answer", &secret, "", "not json").0, 400);
        assert_eq!(post(port, "/api/nope", &secret, "", "{}").0, 404);
        let acted = backend.acted.lock().unwrap();
        assert_eq!(acted.len(), 2);
        assert_eq!(acted[0].0, Action::Send);
    }

    #[test]
    fn a_request_past_the_limits_is_refused() {
        let server = start(fake(), super::super::WebView::Messages, "").unwrap();
        let (port, secret) = (server.port, server.secret.clone());
        let big = format!(
            "POST /api/send HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {COOKIE}={secret}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            BODY_BYTES_MAX + 1
        );
        assert_eq!(send(port, &big).0, 413);
        let headers = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX: {}\r\n\r\n", "a".repeat(HEADER_BYTES_MAX));
        assert_eq!(send(port, &headers).0, 413);
        assert_eq!(send(port, "NONSENSE\r\n\r\n").0, 400);
    }

    #[test]
    fn the_event_stream_wakes_when_the_thread_changes() {
        let backend = fake();
        let server = start(backend.clone(), super::super::WebView::Messages, "").unwrap();
        let (port, secret) = (server.port, server.secret.clone());
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(format!("GET /api/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {COOKIE}={secret}\r\n\r\n").as_bytes()).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        *backend.view.lock().unwrap() = json!({ "title": "renamed" });
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let mut versions = Vec::new();
        for _ in 0..40 {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if let Some(v) = line.strip_prefix("data: ") {
                versions.push(v.trim().parse::<u64>().unwrap());
                if versions.last() == Some(&2) {
                    break;
                }
            }
        }
        assert_eq!(versions, [1, 2], "what the page has, then the change");
        assert!(get(port, "/api/thread", Some(&secret)).1.ends_with(r#"{"title":"renamed"}"#));
    }
}
