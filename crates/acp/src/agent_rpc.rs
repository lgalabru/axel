//! The plumbing the built-in ACP agents share (`claude`, `codex`): JSON
//! lines on a pipe, the ACP client on our stdout, and JSON-RPC errors.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;

use serde_json::{Value, json};

/// The JSON-RPC errors the adapter returns.
#[derive(Debug)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn auth_required() -> Self {
        RpcError { code: -32000, message: "Authentication required".into(), data: None }
    }
    pub fn internal(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        RpcError { code: -32603, message: format!("Internal error: {detail}"), data: Some(json!({ "details": detail })) }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        RpcError { code: -32603, message: message.into(), data: None }
    }
    pub fn not_found(method: &str) -> Self {
        RpcError { code: -32601, message: format!("Method not found: {method}"), data: None }
    }
}

pub(crate) type RpcResult = Result<Value, RpcError>;

/// Writes JSON lines to one pipe, whole lines at a time.
pub(crate) struct Lines<W: Write>(Mutex<W>);

impl<W: Write> Lines<W> {
    pub fn new(w: W) -> Self {
        Lines(Mutex::new(w))
    }

    pub fn send(&self, v: &Value) {
        let mut w = self.0.lock().expect("pipe lock");
        let _ = writeln!(w, "{v}");
        let _ = w.flush();
    }
}

/// The ACP client (Axel's session host), over our stdout.
pub(crate) struct Client {
    pub out: Lines<std::io::Stdout>,
    pub next_id: AtomicU64,
    pub pending: Mutex<HashMap<u64, Sender<Result<Value, Value>>>>,
}

impl Client {
    pub fn respond(&self, id: &Value, result: RpcResult) {
        let msg = match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(e) => {
                let mut err = json!({ "code": e.code, "message": e.message });
                if let Some(d) = e.data {
                    err["data"] = d;
                }
                json!({ "jsonrpc": "2.0", "id": id, "error": err })
            }
        };
        self.out.send(&msg);
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.out.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn update(&self, session_id: &str, update: Value) {
        self.notify("session/update", json!({ "sessionId": session_id, "update": update }));
    }

    /// A request to the client; blocks until it answers.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        self.out.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        rx.recv().unwrap_or_else(|_| Err(json!({ "message": "client went away" })))
    }

    pub fn on_response(&self, msg: &Value) {
        let Some(id) = msg.get("id").and_then(Value::as_u64) else { return };
        if let Some(tx) = self.pending.lock().expect("pending lock").remove(&id) {
            let r = match msg.get("error") {
                Some(e) => Err(e.clone()),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(r);
        }
    }
}

impl Client {
    pub fn stdio() -> Self {
        Client { out: Lines::new(std::io::stdout()), next_id: AtomicU64::new(0), pending: Mutex::new(HashMap::new()) }
    }
}
