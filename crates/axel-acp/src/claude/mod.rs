//! A Claude Code ACP agent, in Rust: `axel claude-acp`.
//!
//! It replaces Zed's `claude-code-acp` (TypeScript, on Anthropic's Claude
//! Agent SDK) for Axel.  Both halves are ported, not reinvented:
//!
//! - **Down, to Claude Code:** what the Agent SDK does.  The `claude` CLI is
//!   started with `--output-format stream-json --input-format stream-json
//!   --permission-prompt-tool stdio …`, prompts go in as `user` messages, and
//!   the SDK's control protocol runs over the same pipes: we send
//!   `initialize` (system prompt, hooks), `interrupt`, `set_permission_mode`,
//!   `set_model`; Claude sends `can_use_tool` and `hook_callback`, and
//!   `control_cancel_request` to withdraw one.
//! - **Up, to the ACP client (Axel's session host):** what `claude-code-acp`
//!   0.16.2's `acp-agent.js` does: the same session/update notifications
//!   (message and thought chunks, tool calls with titles, kinds, diffs and
//!   locations from `tools.js`, plans from TodoWrite, slash commands, mode
//!   changes), the same permission options and outcomes, the same stop
//!   reasons, the same session replay on `session/load`.
//!
//! Deliberate differences, all of which Axel doesn't rely on:
//! - Claude uses its **native** Read / Write / Edit / Bash.  The adapter
//!   swaps them for `mcp__acp__*` tools that call back into the client's
//!   `fs/*` methods (for editors with unsaved buffers); Axel's client reads
//!   and writes the disk just like the native tools, and Claude Code's own
//!   permission rules then apply to its own tool names, so the adapter's
//!   settings-rules PreToolUse hook isn't needed either.
//! - Only what Axel calls is implemented: initialize, session/new,
//!   session/load, session/prompt, session/cancel, session/set_mode,
//!   session/set_model.  No session list / fork / resume, which aren't
//!   advertised.
//!
//! The process is synchronous: one thread reads the client, one per session
//! reads Claude, and each request or control request is answered on its own
//! thread so a permission question never blocks the pipes.

pub mod tools;

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::agent_rpc::{Client, Lines, RpcError, RpcResult};
use crate::usage::{ClaudeMeter, claude_limits, limits_params};

/// One Claude Code process and what we know about its conversation.
struct Session {
    id: String,
    stdin: Lines<ChildStdin>,
    child: Mutex<Child>,
    /// Everything Claude prints that isn't the control protocol.
    messages: Mutex<Receiver<Value>>,
    /// Our control requests waiting for Claude's answer.
    controls: Mutex<HashMap<String, Sender<Value>>>,
    next_control: AtomicU64,
    /// Control requests Claude withdrew (`control_cancel_request`).
    withdrawn: Mutex<HashSet<String>>,
    permission_mode: Mutex<String>,
    cancelled: AtomicBool,
    /// `tool_use` blocks by id, for naming their results.
    tool_uses: Mutex<HashMap<String, Value>>,
    /// Tool uses whose PostToolUse hook should send the tool response.
    hooked: Mutex<HashSet<String>>,
    /// The turn's steering: whether a prompt is running, and the messages
    /// sent into it that Claude hasn't taken in yet (our uuid → the host's
    /// message id). One lock, so a steer can't slip in as the turn ends.
    steering: Mutex<Steering>,
    /// What the turn spent, from Claude's messages and results.
    meter: Mutex<ClaudeMeter>,
}

/// Messages sent into a running turn (`_axel/steer`). Claude takes each in
/// at the turn's next step and echoes it back (`--replay-user-messages`);
/// one that arrives after the turn's last step starts a turn of its own,
/// which the same prompt waits for.
#[derive(Default)]
struct Steering {
    prompting: bool,
    pending: HashMap<String, String>,
}

impl Session {
    /// A control request to Claude; waits for its `control_response`.
    fn control(&self, request: Value, timeout: Duration) -> Result<Value, String> {
        let n = self.next_control.fetch_add(1, Ordering::SeqCst);
        let request_id = format!("axel-{n}");
        let (tx, rx) = mpsc::channel();
        self.controls.lock().expect("controls lock").insert(request_id.clone(), tx);
        self.stdin.send(&json!({ "request_id": request_id, "type": "control_request", "request": request }));
        let resp = rx.recv_timeout(timeout).map_err(|_| {
            self.controls.lock().expect("controls lock").remove(&request_id);
            "Claude Code did not answer".to_string()
        })?;
        if resp.get("subtype").and_then(Value::as_str) == Some("success") {
            Ok(resp.get("response").cloned().unwrap_or(Value::Null))
        } else {
            Err(resp.get("error").and_then(Value::as_str).unwrap_or("control request failed").to_string())
        }
    }

    fn mode(&self) -> String {
        self.permission_mode.lock().expect("mode lock").clone()
    }

    fn set_mode(&self, mode: &str) {
        *self.permission_mode.lock().expect("mode lock") = mode.to_string();
    }
}

impl Session {
    /// End Claude.  Called when the host goes away: the reader and idle
    /// relay threads hold the session until Claude's stdout closes, so a
    /// `Drop` alone would wait on the very process it is meant to end.
    fn kill_child(&self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill_child();
    }
}

struct Agent {
    client: Arc<Client>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    client_caps: Mutex<Value>,
}

/// The `PostToolUse` hook callback id we register with Claude.
const POST_TOOL_USE_HOOK: &str = "hook_0";

/// Run the agent on stdin/stdout until the client closes stdin.
pub fn serve_stdio() -> anyhow::Result<()> {
    let client = Arc::new(Client::stdio());
    let agent = Arc::new(Agent { client: client.clone(), sessions: Mutex::new(HashMap::new()), client_caps: Mutex::new(Value::Null) });
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(method) = msg.get("method").and_then(Value::as_str).map(String::from) else {
            client.on_response(&msg);
            continue;
        };
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let agent = agent.clone();
        std::thread::spawn(move || {
            let result = agent.handle(&method, params);
            match id {
                Some(id) => agent.client.respond(&id, result),
                None => {
                    if let Err(e) = result {
                        eprintln!("[axel claude-acp] {method}: {}", e.message);
                    }
                }
            }
        });
    }
    // The host is gone: no Claude outlives it.  Threads still holding a
    // session keep it alive, so end its process here, not in `Drop`.
    let sessions: Vec<Arc<Session>> = agent.sessions.lock().expect("sessions lock").drain().map(|(_, s)| s).collect();
    for session in sessions {
        session.kill_child();
    }
    Ok(())
}

/// Whether the `bypassPermissions` mode works: not as root, unless sandboxed.
fn allow_bypass() -> bool {
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    !root || std::env::var_os("IS_SANDBOX").is_some()
}

impl Agent {
    fn handle(self: &Arc<Self>, method: &str, params: Value) -> RpcResult {
        match method {
            "initialize" => {
                *self.client_caps.lock().expect("caps lock") = params.get("clientCapabilities").cloned().unwrap_or(Value::Null);
                Ok(json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "promptCapabilities": { "image": true, "embeddedContext": true },
                        "mcpCapabilities": { "http": true, "sse": true },
                        "loadSession": true,
                        // Messages can be sent into a running turn (`_axel/steer`).
                        "_meta": { "axel": { "steer": true } }
                    },
                    "agentInfo": { "name": "axel-claude-acp", "title": "Claude Code", "version": env!("CARGO_PKG_VERSION") },
                    "authMethods": [{ "id": "claude-login", "name": "Log in with Claude Code", "description": "Run `claude /login` in the terminal" }]
                }))
            }
            "authenticate" => Err(RpcError::invalid("Method not implemented.")),
            "session/new" => {
                let home = dirs_home();
                if home.join(".claude.json.backup").exists() && !home.join(".claude.json").exists() {
                    return Err(RpcError::auth_required());
                }
                let resume = params.pointer("/_meta/claudeCode/options/resume").and_then(Value::as_str).map(String::from);
                let (session, reply, commands) = self.create_session(&params, resume)?;
                self.send_commands_later(session, commands);
                Ok(reply)
            }
            "session/load" => {
                let sid = params.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
                let cwd = PathBuf::from(params.get("cwd").and_then(Value::as_str).unwrap_or("."));
                let file = find_session_file(&sid, &cwd).ok_or_else(|| RpcError::invalid("Session not found"))?;
                let (session, reply, commands) = self.create_session(&params, Some(sid.clone()))?;
                replay(&self.client, &sid, &file);
                self.send_commands_later(session, commands);
                Ok(json!({ "modes": reply.get("modes"), "models": reply.get("models") }))
            }
            "session/prompt" => self.prompt(&params),
            "session/cancel" => {
                let s = self.session(&params)?;
                s.cancelled.store(true, Ordering::SeqCst);
                // Messages sent into the turn and not taken in yet would start a
                // turn of their own after the interrupt: withdraw them, and tell
                // the host, which keeps them queued.
                let withdrawn = self.withdraw_all(&s);
                if !withdrawn.is_empty() {
                    self.client.notify("_axel/unsteered", json!({ "sessionId": s.id, "messageIds": withdrawn }));
                }
                s.control(json!({ "subtype": "interrupt" }), Duration::from_secs(30)).map_err(RpcError::invalid)?;
                Ok(Value::Null)
            }
            "_axel/steer" => self.steer(&params),
            "_axel/unsteer" => {
                let s = self.session(&params)?;
                let id = params.get("messageId").and_then(Value::as_str).unwrap_or("");
                Ok(json!({ "withdrawn": self.withdraw(&s, id) }))
            }
            "session/set_model" => {
                let s = self.session(&params)?;
                let model = params.get("modelId").and_then(Value::as_str).unwrap_or("");
                s.control(json!({ "subtype": "set_model", "model": model }), Duration::from_secs(30)).map_err(RpcError::invalid)?;
                Ok(json!({}))
            }
            "session/set_mode" => {
                let s = self.session(&params)?;
                let mode = params.get("modeId").and_then(Value::as_str).unwrap_or("");
                match mode {
                    "default" | "acceptEdits" | "bypassPermissions" | "dontAsk" | "plan" => {
                        s.set_mode(mode);
                        s.control(json!({ "subtype": "set_permission_mode", "mode": mode }), Duration::from_secs(30))
                            .map_err(|e| RpcError::invalid(if e.is_empty() { "Invalid Mode".into() } else { e }))?;
                        Ok(json!({}))
                    }
                    _ => Err(RpcError::invalid("Invalid Mode")),
                }
            }
            other => Err(RpcError::not_found(other)),
        }
    }

    fn session(&self, params: &Value) -> Result<Arc<Session>, RpcError> {
        let sid = params.get("sessionId").and_then(Value::as_str).unwrap_or("");
        self.sessions.lock().expect("sessions lock").get(sid).cloned().ok_or_else(|| RpcError::invalid("Session not found"))
    }

    /// The slash commands, sent once the session reply is out.
    fn send_commands_later(&self, session: String, commands: Value) {
        let client = self.client.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            client.update(&session, json!({ "sessionUpdate": "available_commands_update", "availableCommands": available_commands(&commands) }));
        });
    }

    /// Start Claude Code for a session: `createSession` in the adapter.
    fn create_session(self: &Arc<Self>, params: &Value, resume: Option<String>) -> Result<(String, Value, Value), RpcError> {
        let cwd = PathBuf::from(params.get("cwd").and_then(Value::as_str).unwrap_or("."));
        let session_id = resume.clone().unwrap_or_else(uuid_v4);
        let meta = params.get("_meta").cloned().unwrap_or(Value::Null);
        let user_options = meta.pointer("/claudeCode/options").cloned().unwrap_or(Value::Null);

        // MCP servers: the client's, then any from `_meta.claudeCode.options.mcpServers`.
        let mut mcp = user_options.get("mcpServers").and_then(Value::as_object).cloned().unwrap_or_default();
        for server in params.get("mcpServers").and_then(Value::as_array).into_iter().flatten() {
            let Some(name) = server.get("name").and_then(Value::as_str) else { continue };
            let pairs = |k: &str| -> Option<Value> {
                let a = server.get(k)?.as_array()?;
                Some(Value::Object(
                    a.iter()
                        .filter_map(|e| Some((e.get("name")?.as_str()?.to_string(), e.get("value").cloned().unwrap_or(Value::Null))))
                        .collect(),
                ))
            };
            let entry = match server.get("type").and_then(Value::as_str) {
                Some(kind) => json!({ "type": kind, "url": server.get("url"), "headers": pairs("headers") }),
                None => json!({ "type": "stdio", "command": server.get("command"), "args": server.get("args"), "env": pairs("env") }),
            };
            mcp.insert(name.to_string(), strip_nulls(entry));
        }

        // The system prompt: Claude Code's own, plus an appended part, or a replacement.
        let (system_prompt, append) = match meta.get("systemPrompt") {
            Some(Value::String(s)) => (Some(s.clone()), None),
            Some(v) => (None, v.get("append").and_then(Value::as_str).map(String::from)),
            None => (None, None),
        };

        // `--replay-user-messages`: Claude echoes each user message as it takes
        // it in, which is how a steer is known to have landed.
        let mut args: Vec<String> =
            ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json", "--replay-user-messages"].map(String::from).to_vec();
        if let Some(n) = std::env::var("MAX_THINKING_TOKENS").ok().and_then(|v| v.trim().parse::<i64>().ok()) {
            args.extend(["--max-thinking-tokens".into(), n.to_string()]);
        }
        args.extend(["--permission-prompt-tool".into(), "stdio".into()]);
        if let Some(r) = &resume {
            args.extend(["--resume".into(), r.clone()]);
        }
        // AskUserQuestion stays on: each question is asked as a permission
        // request whose options are the choices (see `ask_questions`).
        args.extend(["--tools".into(), "default".into()]);
        if !mcp.is_empty() {
            args.extend(["--mcp-config".into(), json!({ "mcpServers": mcp }).to_string()]);
        }
        args.extend(["--setting-sources".into(), "user,project,local".into()]);
        args.extend(["--permission-mode".into(), "default".into()]);
        if allow_bypass() {
            args.push("--allow-dangerously-skip-permissions".into());
        }
        args.push("--include-partial-messages".into());
        if resume.is_none() {
            args.extend(["--session-id".into(), session_id.clone()]);
        }
        for (k, v) in user_options.get("extraArgs").and_then(Value::as_object).into_iter().flatten() {
            args.push(format!("--{k}"));
            match v {
                Value::Null => {}
                Value::String(s) => args.push(s.clone()),
                other => args.push(other.to_string()),
            }
        }

        let exe = std::env::var("CLAUDE_CODE_EXECUTABLE").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "claude".into());
        let mut cmd = if [".js", ".mjs", ".cjs"].iter().any(|ext| exe.ends_with(ext)) {
            let mut c = Command::new("node");
            c.arg(&exe);
            c
        } else {
            Command::new(&exe)
        };
        cmd.args(&args).current_dir(&cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        if std::env::var_os("CLAUDE_CODE_ENTRYPOINT").is_none() {
            cmd.env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
        }
        cmd.env_remove("NODE_OPTIONS");
        if std::env::var_os("DEBUG_CLAUDE_AGENT_SDK").is_some() {
            cmd.env("DEBUG", "1");
        } else {
            cmd.env_remove("DEBUG");
        }
        let mut child = cmd.spawn().map_err(|e| RpcError::internal(format!("starting {exe}: {e}")))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel();
        let session = Arc::new(Session {
            id: session_id.clone(),
            stdin: Lines::new(stdin),
            child: Mutex::new(child),
            messages: Mutex::new(rx),
            controls: Mutex::new(HashMap::new()),
            next_control: AtomicU64::new(0),
            withdrawn: Mutex::new(HashSet::new()),
            permission_mode: Mutex::new("default".into()),
            cancelled: AtomicBool::new(false),
            tool_uses: Mutex::new(HashMap::new()),
            hooked: Mutex::new(HashSet::new()),
            steering: Mutex::new(Steering::default()),
            meter: Mutex::new(ClaudeMeter::default()),
        });
        self.spawn_reader(session.clone(), stdout, tx);
        self.spawn_idle_relay(session.clone());
        self.sessions.lock().expect("sessions lock").insert(session_id.clone(), session.clone());

        let mut init = json!({
            "subtype": "initialize",
            "hooks": { "PostToolUse": [{ "hookCallbackIds": [POST_TOOL_USE_HOOK] }] }
        });
        if let Some(sp) = system_prompt {
            init["systemPrompt"] = Value::String(sp);
        }
        if let Some(a) = append {
            init["appendSystemPrompt"] = Value::String(a);
        }
        let init = session.control(init, Duration::from_secs(120)).map_err(RpcError::internal)?;

        // The model: the settings' `model` when it names one Claude offers, else the first.
        let models = init.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
        let wanted = settings_model(&cwd);
        let current = wanted
            .as_deref()
            .and_then(|want| models.iter().find(|m| model_matches(m, want)))
            .or_else(|| models.first())
            .cloned();
        if let Some(value) = current.as_ref().and_then(|m| m.get("value")).and_then(Value::as_str) {
            let _ = session.control(json!({ "subtype": "set_model", "model": value }), Duration::from_secs(30));
        }
        let models_reply = json!({
            "availableModels": models.iter().map(|m| json!({ "modelId": m.get("value"), "name": m.get("displayName"), "description": m.get("description") })).collect::<Vec<_>>(),
            "currentModelId": current.as_ref().and_then(|m| m.get("value")),
        });

        let mut modes = vec![
            json!({ "id": "default", "name": "Default", "description": "Standard behavior, prompts for dangerous operations" }),
            json!({ "id": "acceptEdits", "name": "Accept Edits", "description": "Auto-accept file edit operations" }),
            json!({ "id": "plan", "name": "Plan Mode", "description": "Planning mode, no actual tool execution" }),
            json!({ "id": "dontAsk", "name": "Don't Ask", "description": "Don't prompt for permissions, deny if not pre-approved" }),
        ];
        if allow_bypass() {
            modes.push(json!({ "id": "bypassPermissions", "name": "Bypass Permissions", "description": "Bypass all permission checks" }));
        }
        let reply = json!({
            "sessionId": session_id,
            "models": models_reply,
            "modes": { "currentModeId": "default", "availableModes": modes },
        });
        let commands = init.get("commands").cloned().unwrap_or(Value::Array(vec![]));
        Ok((session_id, reply, commands))
    }

    /// Read Claude's stdout: control traffic is answered here, the rest goes
    /// to whoever is running a prompt.
    fn spawn_reader(self: &Arc<Self>, session: Arc<Session>, stdout: std::process::ChildStdout, tx: Sender<Value>) {
        let agent = self.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                match msg.get("type").and_then(Value::as_str) {
                    Some("control_response") => {
                        let resp = msg.get("response").cloned().unwrap_or(Value::Null);
                        let id = resp.get("request_id").and_then(Value::as_str).unwrap_or("").to_string();
                        if let Some(waiter) = session.controls.lock().expect("controls lock").remove(&id) {
                            let _ = waiter.send(resp);
                        }
                    }
                    Some("control_request") => {
                        let (agent, session) = (agent.clone(), session.clone());
                        std::thread::spawn(move || agent.answer_control(&session, &msg));
                    }
                    Some("control_cancel_request") => {
                        if let Some(id) = msg.get("request_id").and_then(Value::as_str) {
                            session.withdrawn.lock().expect("withdrawn lock").insert(id.to_string());
                        }
                    }
                    Some("keep_alive") | Some("streamlined_text") | Some("streamlined_tool_use_summary") => {}
                    _ => {
                        if tx.send(msg).is_err() {
                            break;
                        }
                    }
                }
            }
            // Claude exited: anyone waiting on a control request won't get an answer.
            session.controls.lock().expect("controls lock").clear();
        });
    }

    /// Answer one control request from Claude.
    fn answer_control(&self, session: &Session, msg: &Value) {
        let request_id = msg.get("request_id").cloned().unwrap_or(Value::Null);
        let request = msg.get("request").cloned().unwrap_or(Value::Null);
        let result: Result<Value, String> = match request.get("subtype").and_then(Value::as_str) {
            Some("can_use_tool") => self.can_use_tool(session, &request).map(|mut r| {
                r["toolUseID"] = request.get("tool_use_id").cloned().unwrap_or(Value::Null);
                r
            }),
            Some("hook_callback") => Ok(self.hook_callback(session, &request)),
            Some("elicitation") => Ok(self.elicitation(session, &request)),
            Some(other) => Err(format!("Unsupported control request subtype: {other}")),
            None => Err("Unsupported control request subtype: undefined".into()),
        };
        let id = request_id.as_str().unwrap_or("").to_string();
        let withdrawn = session.withdrawn.lock().expect("withdrawn lock").remove(&id);
        let response = match result {
            Ok(_) if withdrawn => json!({ "subtype": "error", "request_id": request_id, "error": "Tool use aborted" }),
            Ok(r) => json!({ "subtype": "success", "request_id": request_id, "response": r }),
            Err(e) => json!({ "subtype": "error", "request_id": request_id, "error": e }),
        };
        session.stdin.send(&json!({ "type": "control_response", "response": response }));
    }

    /// `canUseTool` from the adapter: ExitPlanMode asks how to proceed;
    /// bypass and accept-edits allow on their own; anything else asks.
    fn can_use_tool(&self, session: &Session, request: &Value) -> Result<Value, String> {
        let tool = request.get("tool_name").and_then(Value::as_str).unwrap_or("");
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        let tool_use_id = request.get("tool_use_id").cloned().unwrap_or(Value::Null);
        let suggestions = request.get("permission_suggestions").filter(|v| !v.is_null()).cloned();
        let title = tools::tool_info(tool, &input).get("title").cloned().unwrap_or(Value::Null);
        let tool_call = json!({ "toolCallId": tool_use_id, "rawInput": input, "title": title });
        // A request Claude withdrew while we asked is answered as aborted by `answer_control`.

        if tool == "AskUserQuestion" {
            return self.ask_questions(session, &tool_use_id, input);
        }

        if tool == "ExitPlanMode" {
            let outcome = self.ask(
                session,
                &tool_call,
                json!([
                    { "kind": "allow_always", "name": "Yes, and auto-accept edits", "optionId": "acceptEdits" },
                    { "kind": "allow_once", "name": "Yes, and manually approve edits", "optionId": "default" },
                    { "kind": "reject_once", "name": "No, keep planning", "optionId": "plan" }
                ]),
            )?;
            if outcome.is_none() {
                return Err("Tool use aborted".into());
            }
            return Ok(match outcome.as_deref() {
                Some(mode @ ("default" | "acceptEdits")) => {
                    session.set_mode(mode);
                    self.client.update(&session.id, json!({ "sessionUpdate": "current_mode_update", "currentModeId": mode }));
                    json!({
                        "behavior": "allow",
                        "updatedInput": input,
                        "updatedPermissions": suggestions.unwrap_or_else(|| json!([{ "type": "setMode", "mode": mode, "destination": "session" }])),
                    })
                }
                _ => json!({ "behavior": "deny", "message": "User rejected request to exit plan mode.", "interrupt": true }),
            });
        }

        let mode = session.mode();
        if mode == "bypassPermissions" || (mode == "acceptEdits" && is_edit_tool(tool)) {
            return Ok(json!({
                "behavior": "allow",
                "updatedInput": input,
                "updatedPermissions": suggestions.unwrap_or_else(|| always_rule(tool)),
            }));
        }

        let outcome = self.ask(
            session,
            &tool_call,
            json!([
                { "kind": "allow_always", "name": "Always Allow", "optionId": "allow_always" },
                { "kind": "allow_once", "name": "Allow", "optionId": "allow" },
                { "kind": "reject_once", "name": "Reject", "optionId": "reject" }
            ]),
        )?;
        if outcome.is_none() {
            return Err("Tool use aborted".into());
        }
        Ok(match outcome.as_deref() {
            Some("allow_always") => json!({ "behavior": "allow", "updatedInput": input, "updatedPermissions": suggestions.unwrap_or_else(|| always_rule(tool)) }),
            Some("allow") => json!({ "behavior": "allow", "updatedInput": input }),
            _ => json!({ "behavior": "deny", "message": "User refused permission to run tool", "interrupt": true }),
        })
    }

    /// `AskUserQuestion`: each question in turn as a permission request whose
    /// options are its choices (ids `answer:<n>`, which tells the host it is a
    /// question for the owner, never a permission its policy may grant), plus
    /// "none of these".  The labels picked go back as the tool's `answers`.
    fn ask_questions(&self, session: &Session, tool_use_id: &Value, input: Value) -> Result<Value, String> {
        let questions = input.get("questions").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut answers = serde_json::Map::new();
        for (qi, q) in questions.iter().enumerate() {
            let text = q.get("question").and_then(Value::as_str).unwrap_or("").to_string();
            let (tool_call, options, labels) = question_request(tool_use_id, qi, q);
            let Some(picked) = self.ask(session, &tool_call, options)? else {
                return Err("Tool use aborted".into());
            };
            match picked.strip_prefix(ANSWER_PREFIX).and_then(|n| n.parse::<usize>().ok()).and_then(|n| labels.get(n)) {
                Some(label) => {
                    answers.insert(text, Value::String(label.clone()));
                }
                None => {
                    return Ok(json!({
                        "behavior": "deny",
                        "message": "The owner didn't pick any of these options. Ask them in plain text what they want instead.",
                    }));
                }
            }
        }
        let mut updated = input;
        if let Some(obj) = updated.as_object_mut() {
            obj.insert("answers".into(), Value::Object(answers));
        }
        Ok(json!({ "behavior": "allow", "updatedInput": updated }))
    }

    /// An MCP server asks the owner something (`elicitation`), e.g. Axel's
    /// `send` asking whether an agent may reach another.  A form of one
    /// choice is asked like a question (see [`elicitation_choice`]); anything
    /// else is declined, since there is no form to fill in here.
    fn elicitation(&self, session: &Session, request: &Value) -> Value {
        if request.get("mode").and_then(Value::as_str) == Some("url") {
            return json!({ "action": "decline" });
        }
        let message = request.get("message").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let schema = request.get("requested_schema").cloned().unwrap_or(Value::Null);
        let Some((field, choices)) = elicitation_choice(&schema) else {
            return json!({ "action": "decline" });
        };
        let mut options: Vec<Value> = choices
            .iter()
            .enumerate()
            .map(|(i, (_, label))| json!({ "kind": "allow_once", "name": label, "optionId": format!("{ANSWER_PREFIX}{i}") }))
            .collect();
        options.push(json!({ "kind": "reject_once", "name": "Decline", "optionId": format!("{ANSWER_PREFIX}none") }));
        let id = request.get("elicitation_id").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("elicitation-{}", chrono::Utc::now().timestamp_millis()));
        let server = request.get("mcp_server_name").and_then(Value::as_str).unwrap_or("");
        let title = if message.is_empty() { format!("{server} asks") } else { message };
        let tool_call = json!({ "toolCallId": id, "title": title, "kind": "think" });
        match self.ask(session, &tool_call, Value::Array(options)) {
            Ok(Some(picked)) => match picked.strip_prefix(ANSWER_PREFIX).and_then(|n| n.parse::<usize>().ok()).and_then(|n| choices.get(n)) {
                Some((value, _)) => json!({ "action": "accept", "content": { field: value } }),
                None => json!({ "action": "decline" }),
            },
            _ => json!({ "action": "cancel" }),
        }
    }

    /// `session/request_permission`: the option picked, or `None` if cancelled.
    fn ask(&self, session: &Session, tool_call: &Value, options: Value) -> Result<Option<String>, String> {
        let reply = self
            .client
            .request("session/request_permission", json!({ "sessionId": session.id, "toolCall": tool_call, "options": options }))
            .map_err(|e| e.get("message").and_then(Value::as_str).unwrap_or("permission request failed").to_string())?;
        let outcome = reply.get("outcome").cloned().unwrap_or(Value::Null);
        Ok(match outcome.get("outcome").and_then(Value::as_str) {
            Some("selected") => outcome.get("optionId").and_then(Value::as_str).map(String::from),
            _ => None,
        })
    }

    /// The PostToolUse hook: entering plan mode, and the tool's response for
    /// tool calls registered while streaming.
    fn hook_callback(&self, session: &Session, request: &Value) -> Value {
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        if request.get("callback_id").and_then(Value::as_str) == Some(POST_TOOL_USE_HOOK)
            && input.get("hook_event_name").and_then(Value::as_str) == Some("PostToolUse")
        {
            if input.get("tool_name").and_then(Value::as_str) == Some("EnterPlanMode") {
                session.set_mode("plan");
                self.client.update(&session.id, json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }));
            }
            if let Some(id) = request.get("tool_use_id").and_then(Value::as_str)
                && session.hooked.lock().expect("hooked lock").remove(id)
            {
                let name = session.tool_uses.lock().expect("tool uses lock").get(id).and_then(|t| t.get("name").cloned());
                if let Some(name) = name {
                    self.client.update(
                        &session.id,
                        json!({
                            "_meta": { "claudeCode": { "toolResponse": input.get("tool_response"), "toolName": name } },
                            "toolCallId": id,
                            "sessionUpdate": "tool_call_update",
                        }),
                    );
                }
            }
        }
        json!({ "continue": true })
    }

    /// `_axel/steer`: send a message into the running turn, for Claude to take
    /// in at its next step. `accepted: false` when no turn is running (the
    /// host sends it as a prompt instead).
    fn steer(&self, params: &Value) -> RpcResult {
        let s = self.session(params)?;
        let message_id = params.get("messageId").and_then(Value::as_str).unwrap_or("").to_string();
        let mut steering = s.steering.lock().expect("steering lock");
        if !steering.prompting || s.cancelled.load(Ordering::SeqCst) {
            return Ok(json!({ "accepted": false }));
        }
        let uuid = new_uuid();
        let mut message = prompt_to_claude(params);
        message["uuid"] = json!(uuid);
        steering.pending.insert(uuid, message_id);
        s.stdin.send(&message);
        Ok(json!({ "accepted": true }))
    }

    /// Withdraw a message sent into the turn that Claude hasn't taken in yet
    /// (`cancel_async_message`). False when it already has, or isn't ours.
    fn withdraw(&self, s: &Session, message_id: &str) -> bool {
        let uuid = s.steering.lock().expect("steering lock").pending.iter().find(|(_, m)| *m == message_id).map(|(u, _)| u.clone());
        let Some(uuid) = uuid else { return false };
        let cancelled = s
            .control(json!({ "subtype": "cancel_async_message", "message_uuid": uuid }), Duration::from_secs(10))
            .ok()
            .and_then(|r| r.get("cancelled").and_then(Value::as_bool))
            .unwrap_or(false);
        if cancelled {
            s.steering.lock().expect("steering lock").pending.remove(&uuid);
        }
        cancelled
    }

    /// Withdraw every message still waiting to be taken in; their message ids.
    fn withdraw_all(&self, s: &Session) -> Vec<String> {
        let ids: Vec<String> = s.steering.lock().expect("steering lock").pending.values().cloned().collect();
        ids.into_iter().filter(|id| self.withdraw(s, id)).collect()
    }

    /// `prompt` from the adapter: send the user message, relay Claude's
    /// stream until its `result` (and, when messages were sent into the turn
    /// too late for it, until the turn they start ends too).
    fn prompt(&self, params: &Value) -> RpcResult {
        let session = self.session(params)?;
        session.cancelled.store(false, Ordering::SeqCst);
        let result = self.relay_prompt(&session, params);
        {
            let mut steering = session.steering.lock().expect("steering lock");
            steering.prompting = false;
            steering.pending.clear();
        }
        // The prompt's spend goes in its reply; a failed one's counts toward the next.
        let Ok(reply) = result else { return result };
        let mut meter = session.meter.lock().expect("meter lock");
        if let Some((used, size)) = meter.context() {
            self.client.update(&session.id, json!({ "sessionUpdate": "usage_update", "used": used, "size": size }));
        }
        Ok(meter.take().into_reply(reply))
    }

    fn relay_prompt(&self, session: &Arc<Session>, params: &Value) -> RpcResult {
        let rx = session.messages.lock().expect("messages lock");
        // What Claude said on its own since the last turn and the idle relay
        // hasn't shown yet: relayed now, so a stale `result` can't end this
        // prompt before Claude has read it.
        while let Ok(message) = rx.try_recv() {
            let _ = self.relay_message(session, &message);
        }
        // Only now may messages be sent into the turn: one accepted before the
        // prompt itself went out would reach Claude first, as a prompt of its own.
        session.steering.lock().expect("steering lock").prompting = true;
        session.stdin.send(&prompt_to_claude(params));
        loop {
            let Ok(message) = rx.recv() else {
                if session.cancelled.load(Ordering::SeqCst) {
                    return Ok(json!({ "stopReason": "cancelled" }));
                }
                return Err(RpcError::invalid("Session did not end in result"));
            };
            if let Some(end) = self.relay_message(session, &message) {
                return end;
            }
        }
    }

    /// Relay one message from Claude to the client.  `Some` when it ends the
    /// turn (a `result`, or an error that does); the idle relay ignores that.
    fn relay_message(&self, session: &Arc<Session>, message: &Value) -> Option<RpcResult> {
        let sid = session.id.clone();
        match message.get("type").and_then(Value::as_str).unwrap_or("") {
                "system" | "tool_progress" | "tool_use_summary" | "auth_status" => {}
                // How much of the plan's windows the account has used.
                "rate_limit_event" => {
                    let limits = claude_limits(message);
                    if !limits.is_empty() {
                        self.client.notify("_axel/limits", limits_params(&limits));
                    }
                }
                "result" => {
                    session.meter.lock().expect("meter lock").on_result(message);
                    if session.cancelled.load(Ordering::SeqCst) {
                        return Some(Ok(json!({ "stopReason": "cancelled" })));
                    }
                    // A message sent in after the turn's last step: Claude runs
                    // it as a turn of its own. Its result ends this prompt.
                    if !session.steering.lock().expect("steering lock").pending.is_empty() {
                        return None;
                    }
                    let is_error = message.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                    let errors = || {
                        let e: Vec<String> =
                            message.get("errors").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default();
                        if e.is_empty() { message.get("subtype").and_then(Value::as_str).unwrap_or("").to_string() } else { e.join(", ") }
                    };
                    match message.get("subtype").and_then(Value::as_str).unwrap_or("") {
                        "success" => {
                            let result = message.get("result").and_then(Value::as_str).unwrap_or("");
                            if result.contains("Please run /login") {
                                return Some(Err(RpcError::auth_required()));
                            }
                            if is_error {
                                return Some(Err(RpcError::internal(result)));
                            }
                            return Some(Ok(json!({ "stopReason": "end_turn" })));
                        }
                        "error_during_execution" => {
                            if is_error {
                                return Some(Err(RpcError::internal(errors())));
                            }
                            return Some(Ok(json!({ "stopReason": "end_turn" })));
                        }
                        "error_max_budget_usd" | "error_max_turns" | "error_max_structured_output_retries" => {
                            if is_error {
                                return Some(Err(RpcError::internal(errors())));
                            }
                            return Some(Ok(json!({ "stopReason": "max_turn_requests" })));
                        }
                        other => eprintln!("[axel claude-acp] unexpected result subtype {other}"),
                    }
                }
                "stream_event" => {
                    let event = message.get("event").cloned().unwrap_or(Value::Null);
                    let block = match event.get("type").and_then(Value::as_str) {
                        Some("content_block_start") => event.get("content_block").cloned(),
                        Some("content_block_delta") => event.get("delta").cloned(),
                        _ => None,
                    };
                    if let Some(block) = block {
                        let mut cache = session.tool_uses.lock().expect("tool uses lock");
                        for u in to_acp(&Value::Array(vec![block]), "assistant", &mut cache, Some(&session.hooked)) {
                            self.client.update(&sid, u);
                        }
                    }
                }
                // Claude's echo of a user message it took in: a steer that
                // landed, or the prompt itself. Neither is relayed.
                "user" if message.get("isReplay").and_then(Value::as_bool) == Some(true) => {
                    let uuid = message.get("uuid").and_then(Value::as_str).unwrap_or("");
                    let landed = session.steering.lock().expect("steering lock").pending.remove(uuid);
                    if let Some(message_id) = landed {
                        self.client.notify("_axel/steered", json!({ "sessionId": sid, "messageId": message_id }));
                    }
                }
                kind @ ("user" | "assistant") => {
                    if kind == "assistant" {
                        let mut meter = session.meter.lock().expect("meter lock");
                        meter.on_assistant(message);
                        // A long turn's spend shows as it happens, not at its end.
                        if let Some(p) = meter.progress(std::time::Instant::now()) {
                            self.client.notify("_axel/usage", p.into_reply(json!({ "sessionId": sid })));
                        }
                    }
                    if session.cancelled.load(Ordering::SeqCst) {
                        return None;
                    }
                    let msg = message.get("message").cloned().unwrap_or(Value::Null);
                    let content = msg.get("content").cloned().unwrap_or(Value::Null);
                    if let Value::String(text) = &content {
                        if text.contains("<local-command-stdout>") {
                            if text.contains("Context Usage") {
                                let clean = text.replace("<local-command-stdout>", "").replace("</local-command-stdout>", "");
                                let mut cache = session.tool_uses.lock().expect("tool uses lock");
                                for u in to_acp(&Value::String(clean), "assistant", &mut cache, Some(&session.hooked)) {
                                    self.client.update(&sid, u);
                                }
                            }
                            return None;
                        }
                        if text.contains("<local-command-stderr>") {
                            eprintln!("{text}");
                            return None;
                        }
                    }
                    let single_text = content.as_array().is_some_and(|a| a.len() == 1 && a[0].get("type").and_then(Value::as_str) == Some("text"));
                    if kind == "user" && (content.is_string() || single_text) {
                        return None;
                    }
                    if kind == "assistant"
                        && msg.get("model").and_then(Value::as_str) == Some("<synthetic>")
                        && single_text
                        && content[0].get("text").and_then(Value::as_str).is_some_and(|t| t.contains("Please run /login"))
                    {
                        return Some(Err(RpcError::auth_required()));
                    }
                    // Assistant text and thinking already streamed as deltas.
                    let content = if kind == "assistant" {
                        Value::Array(
                            content
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter(|c| !matches!(c.get("type").and_then(Value::as_str), Some("text" | "thinking")))
                                        .cloned()
                                        .collect()
                                })
                                .unwrap_or_default(),
                        )
                    } else {
                        content
                    };
                    let role = msg.get("role").and_then(Value::as_str).unwrap_or(kind).to_string();
                    let mut cache = session.tool_uses.lock().expect("tool uses lock");
                    for u in to_acp(&content, &role, &mut cache, Some(&session.hooked)) {
                        self.client.update(&sid, u);
                    }
                }
                other => eprintln!("[axel claude-acp] unexpected message type {other}"),
            }
        None
    }

    /// Between prompts, Claude can go on by itself: a background task it
    /// started finishes and wakes it, and it reads the output and says what
    /// it found.  Nothing asked, so nothing is reading its stream; left in
    /// the channel, those messages would come out at the next prompt as if
    /// they answered it, and their `result` would end that prompt before
    /// Claude had read it (the real answer then waiting for the prompt after).
    /// This thread relays them as they come, whenever no prompt is running.
    fn spawn_idle_relay(self: &Arc<Self>, session: Arc<Session>) {
        let agent = self.clone();
        std::thread::spawn(move || {
            loop {
                // Polled, never waited on: the receiver is held for an instant,
                // so a prompt starting right now isn't kept waiting for it.
                if !session.steering.lock().expect("steering lock").prompting
                    && let Ok(rx) = session.messages.try_lock()
                {
                    loop {
                        match rx.try_recv() {
                            Ok(message) => {
                                let _ = agent.relay_message(&session, &message);
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => return,
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    }
}

fn is_edit_tool(tool: &str) -> bool {
    // The adapter's EDIT_TOOL_NAMES are its `mcp__acp__Edit` / `Write`; with
    // native tools those are Claude Code's Edit and Write (and NotebookEdit,
    // which Claude Code's own accept-edits mode covers too).
    matches!(tool, "Edit" | "Write" | "NotebookEdit" | "MultiEdit")
}

/// An elicitation form that is one choice: its field and each (value, label).
/// A string `enum` (labels from `enumNames` when given), a `oneOf` of
/// `{const, title}`, or a boolean (Yes / No).
fn elicitation_choice(schema: &Value) -> Option<(String, Vec<(Value, String)>)> {
    let props = schema.get("properties")?.as_object()?;
    if props.len() != 1 {
        return None;
    }
    let (field, p) = props.iter().next()?;
    let choices: Vec<(Value, String)> = if let Some(values) = p.get("enum").and_then(Value::as_array) {
        let names = p.get("enumNames").and_then(Value::as_array);
        values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let label = names.and_then(|n| n.get(i)).and_then(Value::as_str).map(String::from).or_else(|| v.as_str().map(String::from)).unwrap_or_else(|| v.to_string());
                (v.clone(), label)
            })
            .collect()
    } else if let Some(alts) = p.get("oneOf").and_then(Value::as_array) {
        alts.iter()
            .filter_map(|a| {
                let v = a.get("const")?.clone();
                let label = a.get("title").and_then(Value::as_str).map(String::from).or_else(|| v.as_str().map(String::from)).unwrap_or_else(|| v.to_string());
                Some((v, label))
            })
            .collect()
    } else if p.get("type").and_then(Value::as_str) == Some("boolean") {
        vec![(Value::Bool(true), "Yes".into()), (Value::Bool(false), "No".into())]
    } else {
        return None;
    };
    (!choices.is_empty()).then(|| (field.clone(), choices))
}

/// Option ids of a question's choices start with this: the host asks the
/// owner for these, whatever its policy would do with a permission.
pub const ANSWER_PREFIX: &str = "answer:";

/// The permission request for question `qi` of an `AskUserQuestion` call:
/// the tool call (its own id per question, the question as its title), the
/// options (each choice, then "none of these"), and the choices' labels.
fn question_request(tool_use_id: &Value, qi: usize, q: &Value) -> (Value, Value, Vec<String>) {
    let text = q.get("question").and_then(Value::as_str).unwrap_or("").trim();
    let header = q.get("header").and_then(Value::as_str).map(str::trim).filter(|h| !h.is_empty());
    let multi = q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false);
    let mut title = match header {
        Some(h) => format!("{h}: {text}"),
        None => text.to_string(),
    };
    if multi {
        // A permission picks one option; the agent gets the one picked.
        title.push_str(" (pick one)");
    }
    let mut labels = Vec::new();
    let mut options: Vec<Value> = Vec::new();
    for (i, o) in q.get("options").and_then(Value::as_array).into_iter().flatten().enumerate() {
        let label = o.get("label").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let name = match o.get("description").and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) => format!("{label} — {d}"),
            None => label.clone(),
        };
        options.push(json!({ "kind": "allow_once", "name": name, "optionId": format!("{ANSWER_PREFIX}{i}") }));
        labels.push(label);
    }
    options.push(json!({ "kind": "reject_once", "name": "None of these (I'll answer in the chat)", "optionId": format!("{ANSWER_PREFIX}none") }));
    let id = format!("{}#{qi}", tool_use_id.as_str().unwrap_or("question"));
    let tool_call = json!({ "toolCallId": id, "title": title, "kind": "think", "rawInput": q });
    (tool_call, Value::Array(options), labels)
}

fn always_rule(tool: &str) -> Value {
    json!([{ "type": "addRules", "rules": [{ "toolName": tool }], "behavior": "allow", "destination": "session" }])
}

/// ACP session/update payloads for Claude content blocks: `toAcpNotifications`.
fn to_acp(content: &Value, role: &str, cache: &mut HashMap<String, Value>, hooks: Option<&Mutex<HashSet<String>>>) -> Vec<Value> {
    let chunk_kind = if role == "assistant" { "agent_message_chunk" } else { "user_message_chunk" };
    if let Value::String(text) = content {
        return vec![json!({ "sessionUpdate": chunk_kind, "content": { "type": "text", "text": text } })];
    }
    let mut out = Vec::new();
    for chunk in content.as_array().into_iter().flatten() {
        let kind = chunk.get("type").and_then(Value::as_str).unwrap_or("");
        let update = match kind {
            "text" | "text_delta" => Some(json!({ "sessionUpdate": chunk_kind, "content": { "type": "text", "text": chunk.get("text") } })),
            "image" => {
                let src = chunk.get("source").cloned().unwrap_or(Value::Null);
                let b64 = src.get("type").and_then(Value::as_str) == Some("base64");
                let mut c = json!({
                    "type": "image",
                    "data": if b64 { src.get("data").cloned().unwrap_or(Value::Null) } else { json!("") },
                    "mimeType": if b64 { src.get("media_type").cloned().unwrap_or(Value::Null) } else { json!("") },
                });
                if src.get("type").and_then(Value::as_str) == Some("url") {
                    c["uri"] = src.get("url").cloned().unwrap_or(Value::Null);
                }
                Some(json!({ "sessionUpdate": chunk_kind, "content": c }))
            }
            "thinking" | "thinking_delta" => Some(json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": chunk.get("thinking") } })),
            "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                let id = chunk.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                cache.insert(id.clone(), chunk.clone());
                let name = chunk.get("name").and_then(Value::as_str).unwrap_or("");
                let input = chunk.get("input").cloned().unwrap_or(json!({}));
                if name == "TodoWrite" {
                    input.get("todos").filter(|t| t.is_array()).map(|_| json!({ "sessionUpdate": "plan", "entries": tools::plan_entries(&input) }))
                } else {
                    if let Some(h) = hooks {
                        h.lock().expect("hooked lock").insert(id.clone());
                    }
                    let mut u = json!({
                        "_meta": { "claudeCode": { "toolName": name } },
                        "toolCallId": id,
                        "sessionUpdate": "tool_call",
                        "rawInput": input,
                        "status": "pending",
                    });
                    for (k, v) in tools::tool_info(name, &input) {
                        u[k] = v;
                    }
                    Some(u)
                }
            }
            "tool_result"
            | "tool_search_tool_result"
            | "web_fetch_tool_result"
            | "web_search_tool_result"
            | "code_execution_tool_result"
            | "bash_code_execution_tool_result"
            | "text_editor_code_execution_tool_result"
            | "mcp_tool_result" => {
                let id = chunk.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                match cache.get(id) {
                    None => {
                        eprintln!("[axel claude-acp] tool result for an untracked tool use: {id}");
                        None
                    }
                    Some(tool_use) => {
                        let name = tool_use.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                        (name != "TodoWrite").then(|| {
                            let failed = chunk.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                            let mut u = json!({
                                "_meta": { "claudeCode": { "toolName": name } },
                                "toolCallId": id,
                                "sessionUpdate": "tool_call_update",
                                "status": if failed { "failed" } else { "completed" },
                                "rawOutput": chunk.get("content"),
                            });
                            for (k, v) in tools::tool_update(chunk, &name) {
                                u[k] = v;
                            }
                            u
                        })
                    }
                }
            }
            _ => None,
        };
        out.extend(update);
    }
    out
}

/// A Claude user message for an ACP prompt: `promptToClaude`.
/// A random (v4) UUID, the id Claude takes for a user message.
fn new_uuid() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        // No randomness to be had: a clock and a counter are unique enough here.
        static N: AtomicU64 = AtomicU64::new(0);
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        b[..8].copy_from_slice(&(t as u64).to_le_bytes());
        b[8..].copy_from_slice(&N.fetch_add(1, Ordering::SeqCst).to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn prompt_to_claude(params: &Value) -> Value {
    let mut content = Vec::new();
    let mut context = Vec::new();
    for chunk in params.get("prompt").and_then(Value::as_array).into_iter().flatten() {
        match chunk.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let mut text = chunk.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                // `/mcp:server:command args` → `/server:command (MCP) args`
                if let Some(rest) = text.strip_prefix("/mcp:")
                    && let Some((server, tail)) = rest.split_once(':')
                    && !server.is_empty()
                    && !server.contains(char::is_whitespace)
                {
                    let (command, args) = match tail.find(char::is_whitespace) {
                        Some(i) => (&tail[..i], &tail[i..]),
                        None => (tail, ""),
                    };
                    if !command.is_empty() {
                        text = format!("/{server}:{command} (MCP){args}");
                    }
                }
                content.push(json!({ "type": "text", "text": text }));
            }
            "resource_link" => {
                content.push(json!({ "type": "text", "text": uri_link(chunk.get("uri").and_then(Value::as_str).unwrap_or("")) }));
            }
            "resource" => {
                let res = chunk.get("resource").cloned().unwrap_or(Value::Null);
                if let Some(text) = res.get("text").and_then(Value::as_str) {
                    let uri = res.get("uri").and_then(Value::as_str).unwrap_or("");
                    content.push(json!({ "type": "text", "text": uri_link(uri) }));
                    context.push(json!({ "type": "text", "text": format!("\n<context ref=\"{uri}\">\n{text}\n</context>") }));
                }
            }
            "image" => {
                if let Some(data) = chunk.get("data").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                    content.push(json!({ "type": "image", "source": { "type": "base64", "data": data, "media_type": chunk.get("mimeType") } }));
                } else if let Some(uri) = chunk.get("uri").and_then(Value::as_str).filter(|u| u.starts_with("http")) {
                    content.push(json!({ "type": "image", "source": { "type": "url", "url": uri } }));
                }
            }
            _ => {}
        }
    }
    content.extend(context);
    json!({
        "type": "user",
        "message": { "role": "user", "content": content },
        "session_id": params.get("sessionId"),
        "parent_tool_use_id": null,
    })
}

/// `formatUriAsLink`: `[@name](uri)` for file and zed URIs.
fn uri_link(uri: &str) -> String {
    if let Some(path) = uri.strip_prefix("file://") {
        let name = path.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(path);
        return format!("[@{name}]({uri})");
    }
    if uri.starts_with("zed://") {
        let name = uri.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(uri);
        return format!("[@{name}]({uri})");
    }
    uri.to_string()
}

/// `getAvailableSlashCommands`.
fn available_commands(commands: &Value) -> Vec<Value> {
    const UNSUPPORTED: [&str; 7] = ["cost", "keybindings-help", "login", "logout", "output-style:new", "release-notes", "todos"];
    commands
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let raw = c.get("name").and_then(Value::as_str)?;
            let name = match raw.strip_suffix(" (MCP)") {
                Some(n) => format!("mcp:{n}"),
                None => raw.to_string(),
            };
            if UNSUPPORTED.contains(&name.as_str()) {
                return None;
            }
            let hint = match c.get("argumentHint") {
                Some(Value::Array(a)) => Some(a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")),
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            Some(json!({
                "name": name,
                "description": c.get("description").and_then(Value::as_str).unwrap_or(""),
                "input": hint.map(|h| json!({ "hint": h })),
            }))
        })
        .collect()
}

/// Whether a model `{value, displayName}` is the one settings name.
fn model_matches(m: &Value, want: &str) -> bool {
    let value = m.get("value").and_then(Value::as_str).unwrap_or("");
    let display = m.get("displayName").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let w = want.to_lowercase();
    value == want || value.contains(want) || want.contains(value) && !value.is_empty() || display == w || display.contains(&w)
}

fn claude_config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| dirs_home().join(".claude"))
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// The `model` the settings files pick: user, project, local, then managed,
/// later ones winning, as the adapter's SettingsManager merges them.
fn settings_model(cwd: &Path) -> Option<String> {
    let managed = if cfg!(target_os = "macos") { "/Library/Application Support/ClaudeCode/managed-settings.json" } else { "/etc/claude-code/managed-settings.json" };
    [
        claude_config_dir().join("settings.json"),
        cwd.join(".claude").join("settings.json"),
        cwd.join(".claude").join("settings.local.json"),
        PathBuf::from(managed),
    ]
    .iter()
    .rev()
    .filter_map(|p| std::fs::read_to_string(p).ok())
    .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
    .find_map(|v| v.get("model").and_then(Value::as_str).map(String::from))
}

/// `~/.claude/projects/<cwd with non-alphanumerics as ->/<id>.jsonl`, else any project's.
fn find_session_file(session_id: &str, cwd: &Path) -> Option<PathBuf> {
    let projects = claude_config_dir().join("projects");
    let encoded: String = cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let expected = projects.join(encoded).join(format!("{session_id}.jsonl"));
    if expected.exists() {
        return Some(expected);
    }
    std::fs::read_dir(&projects)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join(format!("{session_id}.jsonl")))
        .find(|p| p.exists())
}

/// Send a saved conversation to the client: `replaySessionHistory`.
fn replay(client: &Client, session_id: &str, file: &Path) {
    let Ok(f) = std::fs::File::open(file) else { return };
    let mut cache = HashMap::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else { continue };
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "user" && kind != "assistant" {
            continue;
        }
        if entry.get("isSidechain").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        if entry.get("sessionId").and_then(Value::as_str).is_some_and(|s| s != session_id) {
            continue;
        }
        let Some(message) = entry.get("message") else { continue };
        let role = match message.get("role").and_then(Value::as_str) {
            Some(r @ ("assistant" | "user")) => r,
            _ => continue,
        };
        let content = message.get("content").cloned().unwrap_or(Value::Null);
        if !content.is_string() && !content.is_array() {
            continue;
        }
        for u in to_acp(&content, role, &mut cache, None) {
            client.update(session_id, u);
        }
    }
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).collect::<Map<_, _>>()),
        other => other,
    }
}

/// A random (version 4) UUID, for new Claude sessions.
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b)).is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        b = t.to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_elicitation_of_one_choice_is_asked_like_a_question() {
        // What Axel's `send` asks when an agent may not reach another.
        let form = json!({ "type": "object", "properties": { "answer": { "type": "string", "enum": ["Allow this message", "Always allow", "Don't allow"] } }, "required": ["answer"] });
        let (field, choices) = elicitation_choice(&form).unwrap();
        assert_eq!(field, "answer");
        assert_eq!(choices.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(), vec!["Allow this message", "Always allow", "Don't allow"]);
        let titled = json!({ "properties": { "db": { "oneOf": [{ "const": "pg", "title": "Postgres" }, { "const": "lite", "title": "SQLite" }] } } });
        assert_eq!(elicitation_choice(&titled).unwrap().1, vec![(json!("pg"), "Postgres".to_string()), (json!("lite"), "SQLite".to_string())]);
        let yes_no = json!({ "properties": { "ok": { "type": "boolean" } } });
        assert_eq!(elicitation_choice(&yes_no).unwrap().1[0], (json!(true), "Yes".to_string()));
        // Free text, or several fields: no form here, so it's declined.
        assert!(elicitation_choice(&json!({ "properties": { "name": { "type": "string" } } })).is_none());
        assert!(elicitation_choice(&json!({ "properties": { "a": { "type": "boolean" }, "b": { "type": "boolean" } } })).is_none());
    }

    #[test]
    fn a_question_becomes_a_request_with_its_choices_and_a_way_out() {
        let q = json!({
            "question": "Which database should staging use?",
            "header": "Database",
            "multiSelect": false,
            "options": [
                { "label": "Postgres", "description": "what production runs" },
                { "label": "SQLite" }
            ]
        });
        let (tool_call, options, labels) = question_request(&json!("toolu_1"), 0, &q);
        assert_eq!(tool_call["toolCallId"], "toolu_1#0", "one item per question");
        assert_eq!(tool_call["title"], "Database: Which database should staging use?");
        assert_eq!(labels, vec!["Postgres".to_string(), "SQLite".to_string()]);
        let names: Vec<&str> = options.as_array().unwrap().iter().map(|o| o["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["Postgres — what production runs", "SQLite", "None of these (I'll answer in the chat)"]);
        assert!(options.as_array().unwrap().iter().all(|o| o["optionId"].as_str().unwrap().starts_with(ANSWER_PREFIX)), "the host knows it's a question");
        let (multi, _, _) = question_request(&json!("toolu_1"), 1, &json!({ "question": "Which?", "multiSelect": true, "options": [] }));
        assert_eq!(multi["title"], "Which? (pick one)");
    }

    #[test]
    fn prompts_become_claude_user_messages() {
        let p = json!({
            "sessionId": "s1",
            "prompt": [
                { "type": "text", "text": "/mcp:axel:send hi there" },
                { "type": "resource_link", "uri": "file:///w/src/main.rs" },
                { "type": "resource", "resource": { "uri": "file:///w/a.md", "text": "notes" } },
                { "type": "image", "data": "AAAA", "mimeType": "image/png" },
                { "type": "audio", "data": "x" }
            ]
        });
        let m = prompt_to_claude(&p);
        let c = &m["message"]["content"];
        assert_eq!(c[0]["text"], "/axel:send (MCP) hi there");
        assert_eq!(c[1]["text"], "[@main.rs](file:///w/src/main.rs)");
        assert_eq!(c[2]["text"], "[@a.md](file:///w/a.md)");
        assert_eq!(c[3]["source"], json!({ "type": "base64", "data": "AAAA", "media_type": "image/png" }));
        assert_eq!(c[4]["text"], "\n<context ref=\"file:///w/a.md\">\nnotes\n</context>", "context goes last");
        assert_eq!(c.as_array().unwrap().len(), 5);
        assert_eq!((m["type"].as_str(), m["session_id"].as_str()), (Some("user"), Some("s1")));
    }

    #[test]
    fn blocks_become_acp_updates() {
        let mut cache = HashMap::new();
        let hooked = Mutex::new(HashSet::new());
        let blocks = json!([
            { "type": "text", "text": "hello" },
            { "type": "thinking", "thinking": "hmm" },
            { "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "ls" } },
            { "type": "tool_use", "id": "t2", "name": "TodoWrite", "input": { "todos": [{ "content": "a", "status": "pending" }] } },
            { "type": "redacted_thinking" }
        ]);
        let u = to_acp(&blocks, "assistant", &mut cache, Some(&hooked));
        assert_eq!(u.len(), 4);
        assert_eq!(u[0], json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hello" } }));
        assert_eq!(u[1]["sessionUpdate"], "agent_thought_chunk");
        assert_eq!((u[2]["sessionUpdate"].as_str(), u[2]["title"].as_str(), u[2]["status"].as_str()), (Some("tool_call"), Some("`ls`"), Some("pending")));
        assert_eq!(u[2]["_meta"]["claudeCode"]["toolName"], "Bash");
        assert_eq!(u[3]["sessionUpdate"], "plan");
        assert!(hooked.lock().unwrap().contains("t1") && !hooked.lock().unwrap().contains("t2"));

        let results = json!([
            { "type": "tool_result", "tool_use_id": "t1", "content": "a\nb", "is_error": false },
            { "type": "tool_result", "tool_use_id": "t2", "content": "ok" },
            { "type": "tool_result", "tool_use_id": "nope", "content": "?" }
        ]);
        let r = to_acp(&results, "user", &mut cache, None);
        assert_eq!(r.len(), 1, "TodoWrite and untracked results send nothing");
        assert_eq!((r[0]["status"].as_str(), r[0]["content"][0]["content"]["text"].as_str()), (Some("completed"), Some("a\nb")));
        assert_eq!(to_acp(&json!("hi"), "user", &mut cache, None)[0]["sessionUpdate"], "user_message_chunk");
    }

    #[test]
    fn slash_commands_drop_unsupported_and_rename_mcp() {
        let c = available_commands(&json!([
            { "name": "compact", "description": "Compact", "argumentHint": "<instructions>" },
            { "name": "login", "description": "x" },
            { "name": "axel:send (MCP)", "description": "Send", "argumentHint": ["to", "text"] }
        ]));
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["input"]["hint"], "<instructions>");
        assert_eq!((c[1]["name"].as_str(), c[1]["input"]["hint"].as_str()), (Some("mcp:axel:send"), Some("to text")));
    }

    #[test]
    fn models_match_like_the_adapter() {
        let m = json!({ "value": "claude-opus-5-5", "displayName": "Opus 5.5" });
        assert!(model_matches(&m, "opus") && model_matches(&m, "claude-opus-5-5") && model_matches(&m, "Opus 5.5"));
        assert!(!model_matches(&m, "haiku"));
    }

    #[test]
    fn session_ids_are_uuids() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
    }
}
