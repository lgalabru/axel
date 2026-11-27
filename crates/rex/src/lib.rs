//! axel-rex: a client for the Rex terminal multiplexer that shells out to the
//! `rex` CLI (`rex api call -s SESSION METHOD JSON`).  No WebSocket yet.
//!
//! Every Rex terminal exports `REX_SERVER` and `REX_BLOCK`, and Rex before
//! 0.1.0-preview.23 also `REX_SESSION`; [`detect`] reads them and asks Rex for
//! the session when it isn't set.  All request bodies are built by pure functions in
//! [`payload`] so they can be unit-tested without a server, and the command
//! runner is injectable through [`Runner`].
//!
//! ```no_run
//! let rex = axel_rex::Rex::new(axel_rex::detect().expect("not inside Rex"));
//! let layer = rex.new_floating_terminal(
//!     "axel", [0.15, 0.10, 0.70, 0.80], &["axel".into(), "ui".into(), "launch".into()],
//!     None, &[],
//! )?;
//! rex.close_layer(&layer.layer_id)?;
//! # Ok::<(), axel_rex::Error>(())
//! ```

pub mod control;
pub mod osc;
pub mod payload;
pub mod prompt;
pub mod resume;

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where we are inside Rex, from the environment of the current terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RexEnv {
    /// `REX_SERVER`: the server address, usually a `unix://` socket URL.
    pub server: Option<String>,
    /// `REX_SESSION`: the session this terminal belongs to (`session:<uuid>`).
    pub session: String,
    /// `REX_BLOCK`: the block this terminal is (`block:<uuid>`).
    pub block: String,
}

/// Read [`RexEnv`] from the current terminal's environment.  `None` when not
/// running inside a Rex terminal, i.e. when `REX_BLOCK` is missing or empty.
///
/// Rex 0.1.0-preview.23 stopped exporting `REX_SESSION`: a terminal finds its
/// session from `REX_BLOCK` (#42).  Without `REX_SESSION`, the session comes
/// from `rex whoami --json`, which answers for the calling block.  It is asked
/// each time rather than cached, so it stays right when a pane moves to
/// another session.
pub fn detect() -> Option<RexEnv> {
    let var = |key: &str| std::env::var(key).ok();
    let server = var("REX_SERVER").filter(|v| !v.trim().is_empty());
    detect_with(&var, &CliRunner::new(server))
}

/// [`detect`] over an injected environment and `rex` runner, for tests.
fn detect_with(var: &dyn Fn(&str) -> Option<String>, runner: &dyn Runner) -> Option<RexEnv> {
    let non_empty = |key: &str| var(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let block = non_empty("REX_BLOCK")?;
    let session = match non_empty("REX_SESSION") {
        Some(session) => session,
        // Rex accepts a block id anywhere a session goes since preview.23,
        // so commands still reach the right session if `whoami` can't answer.
        None => session_of_block(runner, &block).unwrap_or_else(|| block.clone()),
    };
    assert!(!session.is_empty() && !block.is_empty());
    Some(RexEnv { server: non_empty("REX_SERVER"), session, block })
}

/// The session holding `block`, from `rex whoami --json`.  `None` when `rex`
/// fails, answers without a session, or answers for a different block.
fn session_of_block(runner: &dyn Runner, block: &str) -> Option<String> {
    let out = runner.run(&["whoami".to_string(), "--json".to_string()]).ok()?;
    let who: Value = serde_json::from_str(&out).ok()?;
    if who.get("block_id").and_then(Value::as_str) != Some(block) {
        return None;
    }
    let session = who.get("session_id").and_then(Value::as_str)?;
    session.starts_with("session:").then(|| session.to_string())
}

/// Errors from the Rex client.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The `rex` binary could not be started at all.
    #[error("could not run `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    /// `rex` exited non-zero; `stderr` holds its diagnostic.
    #[error("rex {method} failed: {}", stderr.trim())]
    Rex { method: String, stderr: String },
    /// `rex` printed something that is not JSON.
    #[error("rex {method} returned invalid JSON: {source}")]
    Json {
        method: String,
        #[source]
        source: serde_json::Error,
    },
    /// The JSON was valid but missed a field we rely on.
    #[error("rex {method} result is missing {field}")]
    Shape { method: String, field: &'static str },
    /// A caller-side argument problem, detected before anything runs.
    #[error("invalid argument: {0}")]
    Invalid(String),
    /// The control connection couldn't open, or dropped.
    #[error("rex control connection: {0}")]
    Connection(String),
    /// Rex didn't answer in time.
    #[error("rex didn't answer in time")]
    Timeout,
}

/// Runs the `rex` CLI with `args` (everything after the program name) and
/// returns its stdout.  Implemented by [`CliRunner`]; tests substitute a fake.
pub trait Runner {
    fn run(&self, args: &[String]) -> Result<String, Error>;
}

/// The real runner: spawns `rex` (or `$AXEL_REX_BIN`) and captures its output.
#[derive(Clone, Debug)]
pub struct CliRunner {
    program: String,
    server: Option<String>,
}

/// `name` is an executable on `PATH`.
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

/// The `rex` the Rex app installs, where it lives on macOS.
fn installed_rex() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let candidates = [
        std::path::Path::new(&home).join("Library/Application Support/com.superlogical.rex/RexServer/Versions/Current/Rex Server.app/Contents/MacOS/rex"),
        std::path::PathBuf::from("/Applications/Rex.app/Contents/MacOS/rex"),
        std::path::PathBuf::from("/Applications/Rex Beta.app/Contents/MacOS/rex"),
    ];
    candidates.iter().find(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned())
}

/// The `rex` executable Axel runs: `$AXEL_REX_BIN`, else `rex` on `PATH`, else
/// the one the Rex app installs (Rex terminals put it on `PATH`; others don't).
pub fn program() -> String {
    std::env::var("AXEL_REX_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| (!on_path("rex")).then(installed_rex).flatten())
        .unwrap_or_else(|| "rex".to_string())
}

impl CliRunner {
    /// Use `rex` from `PATH` (or `$AXEL_REX_BIN`), talking to `server` if given
    /// (exported to the child as `REX_SERVER`; the CLI reads it from there).
    /// Outside a Rex terminal `rex` is usually not on `PATH` (Rex terminals add
    /// it): then the one the Rex app installs.
    pub fn new(server: Option<String>) -> CliRunner {
        CliRunner { program: program(), server }
    }

    /// The method name an `api call` argv targets, for error messages.
    fn method_of(args: &[String]) -> String {
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == "call" {
                // skip option/value pairs that precede METHOD
                let mut rest = it.clone();
                while let Some(x) = rest.next() {
                    if x == "-s" || x == "--session" || x == "-S" || x == "--server" {
                        rest.next();
                    } else if !x.starts_with('-') {
                        return x.clone();
                    }
                }
            }
        }
        args.join(" ")
    }
}

impl Default for CliRunner {
    fn default() -> Self {
        CliRunner::new(None)
    }
}

/// The most of `rex`'s stdout read for one call. The largest reply is
/// `session.view` of a busy session, tens of KiB; 64 MiB is far past any
/// real one and still keeps a runaway child from exhausting memory.
const STDOUT_BYTES_MAX: u64 = 64 * 1024 * 1024;
/// The most of `rex`'s stderr kept for an error message: a diagnostic is a
/// few lines. The rest is read and dropped so the child never blocks on a
/// full pipe.
const STDERR_BYTES_MAX: u64 = 64 * 1024;

impl Runner for CliRunner {
    fn run(&self, args: &[String]) -> Result<String, Error> {
        let mut cmd = Command::new(&self.program);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(server) = &self.server {
            cmd.env("REX_SERVER", server);
        }
        let spawn_error = |source| Error::Spawn { program: self.program.clone(), source };
        let mut child = cmd.spawn().map_err(spawn_error)?;
        let out = read_output(&mut child).map_err(spawn_error)?;
        if out.is_stdout_cut {
            return Err(Error::Rex {
                method: CliRunner::method_of(args),
                stderr: format!("printed more than {STDOUT_BYTES_MAX} bytes"),
            });
        }
        if !out.status.success() {
            let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            if stderr.trim().is_empty() {
                stderr = format!("exit status {}", out.status);
            }
            return Err(Error::Rex {
                method: CliRunner::method_of(args),
                stderr,
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// What a `rex` child printed, each stream cut to its limit.
struct Output {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Stdout went past [`STDOUT_BYTES_MAX`]; the child was killed.
    is_stdout_cut: bool,
}

/// Read `child`'s stdout and stderr (both piped) up to their limits and
/// wait for it. Stderr is drained on a thread so a child that fills one
/// pipe while we read the other can't deadlock.
fn read_output(child: &mut std::process::Child) -> std::io::Result<Output> {
    use std::io::Read;

    let stderr_pipe = child.stderr.take().expect("stderr is piped by the caller");
    let stderr_reader = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut stderr_pipe = stderr_pipe;
        let mut stderr = Vec::new();
        (&mut stderr_pipe).take(STDERR_BYTES_MAX).read_to_end(&mut stderr)?;
        std::io::copy(&mut stderr_pipe, &mut std::io::sink())?;
        Ok(stderr)
    });

    let stdout_pipe = child.stdout.take().expect("stdout is piped by the caller");
    let mut stdout = Vec::new();
    // One byte past the limit tells "exactly at the limit" from "over it".
    let read_result = stdout_pipe.take(STDOUT_BYTES_MAX + 1).read_to_end(&mut stdout);
    let is_stdout_cut = u64::try_from(stdout.len()).map_or(true, |read_bytes| read_bytes > STDOUT_BYTES_MAX);
    if is_stdout_cut {
        // It would block on a full pipe forever; it already failed us.
        // Killing a child that has just exited fails harmlessly.
        let _ = child.kill();
        stdout.clear();
    }
    let status = child.wait()?;
    let stderr = match stderr_reader.join() {
        Ok(stderr) => stderr?,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    read_result?;
    Ok(Output { status, stdout, stderr, is_stdout_cut })
}

/// A floating layer created by [`Rex::new_floating_terminal`].
/// Result of `Rex::terminal_size`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
    pub cell_width_px: Option<u16>,
    pub cell_height_px: Option<u16>,
}

/// A terminal to the right of another in the same row: `(label, command)`,
/// the left one keeping `keep` of the width.
#[derive(Clone, Debug)]
pub struct Beside<'a> {
    pub label: &'a str,
    pub command: &'a [String],
    pub keep: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    pub layer_id: String,
    pub block_ids: Vec<String>,
}

/// A window created by [`Rex::new_window`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub window_id: String,
    pub block_ids: Vec<String>,
}

/// The flavor of a Rex terminal block.
pub const TERMINAL_FLAVOR: &str = "com.superlogical.terminal.shell";

/// Client for one Rex session.
pub struct Rex {
    env: RexEnv,
    runner: Box<dyn Runner>,
}

impl std::fmt::Debug for Rex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rex")
            .field("env", &self.env)
            .finish_non_exhaustive()
    }
}

impl Rex {
    /// A client that runs the real `rex` CLI.
    pub fn new(env: RexEnv) -> Rex {
        let runner = CliRunner::new(env.server.clone());
        Rex {
            env,
            runner: Box::new(runner),
        }
    }

    /// A client with a custom [`Runner`] (tests, dry runs).
    pub fn with_runner(env: RexEnv, runner: Box<dyn Runner>) -> Rex {
        Rex { env, runner }
    }

    /// The environment this client was built from.
    pub fn env(&self) -> &RexEnv {
        &self.env
    }

    /// `rex events -s SESSION --json`, ready to spawn: one JSON object per line
    /// for every event of this session (`session_event` with `payload.event
    /// {type, payload}`), until killed.  Stdout is piped, stdin and stderr are
    /// dropped.
    pub fn events_command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(program());
        cmd.args(["events", "-s", &self.env.session, "--json"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        if let Some(server) = &self.env.server {
            cmd.env("REX_SERVER", server);
        }
        cmd
    }

    /// `rex api call -s SESSION METHOD JSON`.  The CLI injects `session_id`
    /// into `args` from `-s`, so callers never pass it.  `args` must be a JSON
    /// object (use `json!({})` for none).
    pub fn api_call(&self, method: &str, args: Value) -> Result<Value, Error> {
        self.call(Some(&self.env.session), method, args)
    }

    /// Like [`Rex::api_call`] but without a session scope, for `server.*` methods.
    pub fn server_call(&self, method: &str, args: Value) -> Result<Value, Error> {
        self.call(None, method, args)
    }

    fn call(&self, session: Option<&str>, method: &str, args: Value) -> Result<Value, Error> {
        if !args.is_object() {
            return Err(Error::Invalid(format!(
                "{method}: args must be a JSON object, got {args}"
            )));
        }
        let mut argv: Vec<String> = vec!["api".into(), "call".into()];
        if let Some(s) = session {
            argv.push("-s".into());
            argv.push(s.to_string());
        }
        argv.push(method.to_string());
        argv.push(args.to_string());
        let stdout = self.runner.run(&argv)?;
        if stdout.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&stdout).map_err(|source| Error::Json {
            method: method.to_string(),
            source,
        })
    }

    /// Open a floating terminal layer in the active window running `command`
    /// directly (`shell: "none"`), closing when it exits.  `env` is applied by
    /// prefixing the command with `/usr/bin/env K=V ...` because the block
    /// options schema has no env field.
    pub fn new_floating_terminal(
        &self,
        label: &str,
        bounds: [f64; 4],
        command: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
    ) -> Result<Layer, Error> {
        self.new_floating_terminal_with_focus(label, bounds, command, cwd, env, true)
    }

    /// Close a floating layer and every block in it.
    pub fn close_layer(&self, layer_id: &str) -> Result<(), Error> {
        self.api_call("session.close_layer", payload::close_layer(layer_id))
            .map(drop)
    }

    /// Like `new_floating_terminal`, with an explicit focus flag.
    pub fn new_floating_terminal_with_focus(
        &self,
        label: &str,
        bounds: [f64; 4],
        command: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
        focus: bool,
    ) -> Result<Layer, Error> {
        let args = payload::new_layer_with_focus(label, bounds, command, cwd, env, focus)?;
        let result = self.api_call("session.new_layer", args)?;
        Ok(Layer {
            layer_id: str_field(&result, "session.new_layer", "layer_id")?,
            block_ids: ids_field(&result, "block_ids"),
        })
    }

    /// A terminal block's size: `(columns, rows, cell_width_px, cell_height_px)`.
    /// Pixel sizes are `None` when the server doesn't know them.
    pub fn terminal_size(&self, block_id: &str) -> Result<TerminalSize, Error> {
        let v = self.api_call(
            "com.superlogical.terminal.size",
            payload::terminal_size(block_id),
        )?;
        // A size past u16 is nonsense from the server; saturate rather than
        // wrap it to a small number.
        let n = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_u64())
                .or_else(|| v.get("size").and_then(|s| s.get(k)).and_then(|x| x.as_u64()))
                .map(|x| u16::try_from(x).unwrap_or(u16::MAX))
        };
        Ok(TerminalSize {
            columns: n("columns").unwrap_or(0),
            rows: n("rows").unwrap_or(0),
            cell_width_px: n("cell_width_px"),
            cell_height_px: n("cell_height_px"),
        })
    }

    /// Move or resize a floating layer. `bounds` are `[x, y, w, h]` fractions.
    pub fn resize_layer(&self, layer_id: &str, bounds: [f64; 4]) -> Result<(), Error> {
        self.api_call(
            "session.resize_layer",
            payload::resize_layer(layer_id, bounds),
        )
        .map(drop)
    }

    /// Open a new window (tab) labelled `label` running `command` directly.
    pub fn new_window(
        &self,
        label: &str,
        command: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
        focus: bool,
    ) -> Result<Window, Error> {
        let args = payload::new_window(label, command, cwd, env, focus)?;
        let result = self.api_call("session.new_window", args)?;
        Ok(Window {
            window_id: str_field(&result, "session.new_window", "window_id")?,
            block_ids: ids_field(&result, "block_ids"),
        })
    }

    /// `bottom`, with `beside` (if any) to its right in the same row.
    fn row(bottom: Value, beside: Option<&Beside>, cwd: Option<&Path>, env: &[(String, String)]) -> Result<Value, Error> {
        let Some(b) = beside else { return Ok(bottom) };
        let other = payload::terminal_block(b.label, b.command, cwd, env)?;
        payload::row(bottom, other, b.keep)
    }

    /// Open a window with two terminals, `top` above `bottom`, each
    /// `(label, command)`; the top gets `ratio` of the height.  Block ids come
    /// back top first.
    #[allow(clippy::too_many_arguments)]
    pub fn new_window_stacked(
        &self,
        label: &str,
        top: (&str, &[String]),
        bottom: (&str, &[String]),
        beside: Option<&Beside>,
        ratio: f64,
        cwd: Option<&Path>,
        env: &[(String, String)],
        focus: bool,
    ) -> Result<Window, Error> {
        let t = payload::terminal_block(top.0, top.1, cwd, env)?;
        let b = Self::row(payload::terminal_block(bottom.0, bottom.1, cwd, env)?, beside, cwd, env)?;
        let args = payload::new_window_stacked(label, t, b, ratio, focus)?;
        let result = self.api_call("session.new_window", args)?;
        Ok(Window {
            window_id: str_field(&result, "session.new_window", "window_id")?,
            block_ids: ids_field(&result, "block_ids"),
        })
    }

    /// Split `anchor` (a block or a split) with a column on its right holding
    /// two terminals, `top` above `bottom`, each `(label, command)`.  The
    /// anchor keeps `keep` of the width; `top` gets `stack` of the column's
    /// height.  Block ids come back top first.
    #[allow(clippy::too_many_arguments)]
    pub fn split_right_stacked(
        &self,
        anchor: &str,
        top: (&str, &[String]),
        bottom: (&str, &[String]),
        beside: Option<&Beside>,
        keep: f64,
        stack: f64,
        cwd: Option<&Path>,
        env: &[(String, String)],
        focus: bool,
    ) -> Result<Vec<String>, Error> {
        let t = payload::terminal_block(top.0, top.1, cwd, env)?;
        let b = Self::row(payload::terminal_block(bottom.0, bottom.1, cwd, env)?, beside, cwd, env)?;
        let args = payload::new_split_right_stacked(anchor, t, b, keep, stack, focus)?;
        let result = self.api_call("session.new_split", args)?;
        let ids = ids_field(&result, "block_ids");
        if ids.len() < 2 {
            return Err(Error::Invalid(format!("session.new_split returned {} blocks, wanted 2", ids.len())));
        }
        Ok(ids)
    }

    /// One terminal below `anchor` (which keeps `ratio` of the height), with
    /// `beside` to its right in the same row.  Block ids come back left first.
    #[allow(clippy::too_many_arguments)]
    pub fn split_below_beside(
        &self,
        anchor: &str,
        label: &str,
        command: &[String],
        beside: Option<&Beside>,
        ratio: f64,
        cwd: Option<&Path>,
        env: &[(String, String)],
        focus: bool,
    ) -> Result<Vec<String>, Error> {
        let block = Self::row(payload::terminal_block(label, command, cwd, env)?, beside, cwd, env)?;
        let args = payload::new_split_below(anchor, block, ratio, focus)?;
        let result = self.api_call("session.new_split", args)?;
        Ok(ids_field(&result, "block_ids"))
    }

    /// Run `command` in a new terminal that isn't in any window (detached):
    /// it runs, reports its status, and shows nowhere until it is moved into
    /// a window (`session.move_block`).  Returns the block's id.
    pub fn new_detached_terminal(
        &self,
        label: &str,
        command: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
    ) -> Result<String, Error> {
        // `terminal_block` is a layout node (`{"block": {flavor, label, options}}`);
        // `session.new_block` takes the block's fields themselves.
        let node = payload::terminal_block(label, command, cwd, env)?;
        let block = node.get("block").cloned().unwrap_or(node);
        let result = self.api_call("session.new_block", block)?;
        str_field(&result, "session.new_block", "block_id")
    }

    /// Run `command` in a new terminal right of `anchor` (a block or a split),
    /// which keeps `keep` of the width.  Returns the new block's id.
    #[allow(clippy::too_many_arguments)]
    pub fn split_right(
        &self,
        anchor: &str,
        label: &str,
        command: &[String],
        keep: f64,
        cwd: Option<&Path>,
        env: &[(String, String)],
        focus: bool,
    ) -> Result<String, Error> {
        let block = payload::terminal_block(label, command, cwd, env)?;
        let args = payload::new_split_right(anchor, block, keep, focus)?;
        let result = self.api_call("session.new_split", args)?;
        ids_field(&result, "block_ids")
            .into_iter()
            .next()
            .ok_or_else(|| Error::Invalid("session.new_split returned no block".into()))
    }

    /// Split `anchor` and run `command` in a new terminal below it; the anchor
    /// keeps `ratio` of the height.  Returns the new block's id.
    #[allow(clippy::too_many_arguments)]
    pub fn split_below(
        &self,
        anchor: &str,
        label: &str,
        command: &[String],
        ratio: f64,
        cwd: Option<&Path>,
        env: &[(String, String)],
        focus: bool,
    ) -> Result<String, Error> {
        let block = payload::terminal_block(label, command, cwd, env)?;
        let args = payload::new_split_below(anchor, block, ratio, focus)?;
        let result = self.api_call("session.new_split", args)?;
        ids_field(&result, "block_ids")
            .into_iter()
            .next()
            .ok_or_else(|| Error::Invalid("session.new_split returned no block".into()))
    }

    /// Relabel a block (≤128 chars, no control characters; Rex trims whitespace).
    pub fn set_block_label(&self, block_id: &str, label: &str) -> Result<(), Error> {
        self.api_call(
            "session.set_block_label",
            payload::set_block_label(block_id, label),
        )
        .map(drop)
    }

    /// Relabel a window.
    pub fn set_window_label(&self, window_id: &str, label: &str) -> Result<(), Error> {
        self.api_call(
            "session.set_window_label",
            payload::set_window_label(window_id, label),
        )
        .map(drop)
    }

    /// Make a window the active one.
    pub fn focus_window(&self, window_id: &str) -> Result<(), Error> {
        self.api_call("session.focus_window", payload::focus_window(window_id))
            .map(drop)
    }

    /// `session.view`: windows, layers and blocks of this session.
    pub fn view(&self) -> Result<Value, Error> {
        self.api_call("session.view", serde_json::json!({}))
    }

    /// The terminal's processes (`com.superlogical.terminal.process`): its
    /// `child`, the `foreground` process and that one's `ancestors`.
    pub fn process(&self, block_id: &str) -> Result<Value, Error> {
        self.api_call("com.superlogical.terminal.process", serde_json::json!({"block_id": block_id, "args": {}}))
    }

    /// OSC 7501 records a terminal block holds (`com.superlogical.terminal.program_status`).
    /// The result is `{"records": [...]}`.
    pub fn program_status(&self, block_id: &str) -> Result<Value, Error> {
        self.api_call(
            "com.superlogical.terminal.program_status",
            payload::program_status(block_id),
        )
    }

    /// `server.status`: everything the server says about itself.
    pub fn server_status(&self) -> Result<Value, Error> {
        self.server_call("server.status", serde_json::json!({}))
    }

    /// The server's `host_id` (identifies machine + account; same files ⇒ same id).
    pub fn host_id(&self) -> Result<String, Error> {
        str_field(&self.server_status()?, "server.status", "host_id")
    }

    /// Put `pick` to the owner in every connected Rex app and wait for the
    /// first answer. `should_stop` is checked every [`ASK_POLL_MS`]; when it
    /// says yes, Axel stops waiting and Rex closes the prompt everywhere.
    pub fn ask(&self, pick: &prompt::Pick, should_stop: &dyn Fn() -> bool) -> Result<prompt::Answer, Error> {
        let request = prompt::action_args(pick);
        let wait_seconds = pick.timeout_seconds + ASK_SLACK_SECONDS;
        let mut cmd = Command::new(program());
        cmd.args(["--timeout", &format!("{wait_seconds}s"), "api", "call", "client.action", &request.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(server) = &self.env.server {
            cmd.env("REX_SERVER", server);
        }
        let spawn_error = |source| Error::Spawn { program: program(), source };
        let mut child = cmd.spawn().map_err(spawn_error)?;
        let Some(out) = wait_or_stop(&mut child, std::time::Duration::from_secs(u64::from(wait_seconds)), should_stop).map_err(spawn_error)? else {
            return Ok(prompt::Answer::Unanswered(prompt::Unanswered::Withdrawn));
        };
        let method = "client.action".to_string();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            // Rex's wording: "the configuration file defines no action \"axel_ask\"".
            if stderr.contains("defines no action") {
                return Ok(prompt::Answer::Unanswered(prompt::Unanswered::NotInstalled));
            }
            return Err(Error::Rex { method, stderr });
        }
        let reply: Value = serde_json::from_slice(&out.stdout).map_err(|source| Error::Json { method: method.clone(), source })?;
        let result = reply.get("result").ok_or(Error::Shape { method: method.clone(), field: "result" })?;
        prompt::parse_result(result, pick).map_err(|why| Error::Rex { method, stderr: why })
    }
}

/// How long [`Rex::ask`] waits past the prompt's own timeout before it gives
/// up on Rex: the action's own 30 s budget, which prompt time doesn't count.
const ASK_SLACK_SECONDS: u32 = 30;
/// How often [`Rex::ask`] checks whether to stop waiting. A fifth of a second
/// is quicker than a person notices and costs one closure call.
const ASK_POLL_MS: u64 = 200;

/// Wait for `child` until it exits, `limit` passes, or `should_stop` says so.
/// `None` when stopped or past the limit: the child is killed, which makes
/// Rex close its prompt. Output is drained on threads so it can't block.
fn wait_or_stop(child: &mut std::process::Child, limit: std::time::Duration, should_stop: &dyn Fn() -> bool) -> std::io::Result<Option<Output>> {
    use std::io::Read;
    let drain = |pipe: Option<Box<dyn Read + Send>>, max: u64| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(pipe) = pipe {
                // A read error leaves what was read; the exit status still decides.
                let _ = pipe.take(max).read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), STDOUT_BYTES_MAX);
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>), STDERR_BYTES_MAX);
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if should_stop() || std::time::Instant::now() >= deadline {
            // Killing an exited child fails harmlessly; wait reaps it either way.
            let _ = child.kill();
            child.wait()?;
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(ASK_POLL_MS));
    };
    let join = |handle: std::thread::JoinHandle<Vec<u8>>| handle.join().unwrap_or_default();
    Ok(Some(Output { status, stdout: join(stdout), stderr: join(stderr), is_stdout_cut: false }))
}

fn str_field(v: &Value, method: &str, field: &'static str) -> Result<String, Error> {
    v.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Shape {
            method: method.to_string(),
            field,
        })
}

fn ids_field(v: &Value, field: &str) -> Vec<String> {
    v.get(field)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    /// Records every argv and replies from a queue.
    struct Fake {
        calls: RefCell<Vec<Vec<String>>>,
        replies: RefCell<Vec<Result<String, Error>>>,
    }

    impl Fake {
        fn new(replies: Vec<Result<String, Error>>) -> Fake {
            let mut replies = replies;
            replies.reverse();
            Fake {
                calls: RefCell::new(Vec::new()),
                replies: RefCell::new(replies),
            }
        }
    }

    impl Runner for Fake {
        fn run(&self, args: &[String]) -> Result<String, Error> {
            self.calls.borrow_mut().push(args.to_vec());
            self.replies
                .borrow_mut()
                .pop()
                .unwrap_or_else(|| Ok("{}".to_string()))
        }
    }

    fn env() -> RexEnv {
        RexEnv {
            server: Some("unix:///tmp/rex.sock".into()),
            session: "session:abc".into(),
            block: "block:def".into(),
        }
    }

    /// Build a client around a fake and hand back a way to inspect its calls.
    fn client(replies: Vec<Result<String, Error>>) -> (Rex, std::rc::Rc<Fake>) {
        let fake = std::rc::Rc::new(Fake::new(replies));
        struct Shared(std::rc::Rc<Fake>);
        impl Runner for Shared {
            fn run(&self, args: &[String]) -> Result<String, Error> {
                self.0.run(args)
            }
        }
        (
            Rex::with_runner(env(), Box::new(Shared(fake.clone()))),
            fake,
        )
    }

    #[test]
    fn env_accessor() {
        let rex = Rex::new(env());
        assert_eq!(rex.env().session, "session:abc");
        assert_eq!(rex.env().block, "block:def");
    }

    /// An environment from fixed pairs, as `detect_with` reads it.
    fn vars(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let pairs = pairs.to_vec();
        move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
    }

    const WHOAMI: &str = r#"{"session_id": "session:s1", "window_id": "window:w1", "block_id": "block:b1"}"#;

    #[test]
    fn detect_uses_rex_session_when_set() {
        let fake = Fake::new(vec![]);
        let env = detect_with(&vars(&[("REX_SESSION", "session:s0"), ("REX_BLOCK", "block:b1")]), &fake).unwrap();
        assert_eq!((env.session.as_str(), env.block.as_str()), ("session:s0", "block:b1"));
        assert!(fake.calls.borrow().is_empty(), "no need to ask rex");
    }

    #[test]
    fn detect_asks_rex_for_the_session_when_only_the_block_is_set() {
        // What Rex 0.1.0-preview.23 terminals export (#42).
        let fake = Fake::new(vec![Ok(WHOAMI.into())]);
        let env = detect_with(&vars(&[("REX_SERVER", "unix:///tmp/rex.sock"), ("REX_BLOCK", "block:b1")]), &fake).unwrap();
        assert_eq!(env.session, "session:s1");
        assert_eq!(env.block, "block:b1");
        assert_eq!(env.server.as_deref(), Some("unix:///tmp/rex.sock"));
        assert_eq!(fake.calls.borrow().as_slice(), &[vec!["whoami".to_string(), "--json".to_string()]]);
    }

    #[test]
    fn detect_falls_back_to_the_block_when_rex_cannot_say() {
        let failing = Error::Rex { method: "whoami".into(), stderr: "no server".into() };
        let other_block = r#"{"session_id": "session:s9", "block_id": "block:other"}"#;
        for reply in [Err(failing), Ok(other_block.to_string()), Ok("{}".to_string()), Ok("not json".to_string())] {
            let fake = Fake::new(vec![reply]);
            let env = detect_with(&vars(&[("REX_BLOCK", "block:b1")]), &fake).unwrap();
            assert_eq!(env.session, "block:b1");
        }
    }

    #[test]
    fn detect_is_none_outside_rex() {
        let fake = Fake::new(vec![Ok(WHOAMI.into())]);
        assert!(detect_with(&vars(&[]), &fake).is_none());
        assert!(detect_with(&vars(&[("REX_SESSION", "session:s0"), ("REX_BLOCK", "  ")]), &fake).is_none());
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn api_call_argv_shape() {
        let (rex, fake) = client(vec![Ok(r#"{"revision": 3}"#.into())]);
        let v = rex.api_call("session.view", json!({})).unwrap();
        assert_eq!(v, json!({"revision": 3}));
        let calls = fake.calls.borrow();
        assert_eq!(
            calls[0],
            vec!["api", "call", "-s", "session:abc", "session.view", "{}"]
        );
    }

    #[test]
    fn server_call_has_no_session_scope() {
        let (rex, fake) =
            client(vec![Ok(r#"{"pid":1,"host_id":"host-1","instance_id":"i","started_at":"t","exit_empty":false,"session_count":1,"config":{"generation":1,"hash":""}}"#.into())]);
        assert_eq!(rex.host_id().unwrap(), "host-1");
        assert_eq!(
            fake.calls.borrow()[0],
            vec!["api", "call", "server.status", "{}"]
        );
    }

    #[test]
    fn host_id_missing_is_shape_error() {
        let (rex, _) = client(vec![Ok(r#"{"pid":1}"#.into())]);
        assert!(matches!(
            rex.host_id(),
            Err(Error::Shape {
                field: "host_id",
                ..
            })
        ));
    }

    #[test]
    fn non_object_args_rejected_before_running() {
        let (rex, fake) = client(vec![]);
        assert!(matches!(
            rex.api_call("x", json!([1])),
            Err(Error::Invalid(_))
        ));
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn runner_failure_propagates() {
        let (rex, _) = client(vec![Err(Error::Rex {
            method: "session.view".into(),
            stderr: "boom".into(),
        })]);
        match rex.view() {
            Err(Error::Rex { method, stderr }) => {
                assert_eq!(method, "session.view");
                assert_eq!(stderr, "boom");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn invalid_json_is_json_error_and_empty_is_null() {
        let (rex, _) = client(vec![Ok("not json".into()), Ok("  \n".into())]);
        assert!(matches!(rex.view(), Err(Error::Json { .. })));
        assert_eq!(rex.view().unwrap(), Value::Null);
    }

    #[test]
    fn new_floating_terminal_payload_and_result() {
        let (rex, fake) = client(vec![Ok(
            r#"{"layer_id":"layer:1","block_ids":["block:1", null],"revision":9}"#.into(),
        )]);
        let layer = rex
            .new_floating_terminal(
                "axel",
                [0.15, 0.10, 0.70, 0.80],
                &["axel".into(), "ui".into(), "launch".into()],
                Some(Path::new("/work")),
                &[("AXEL_DB".into(), "/tmp/a.db".into())],
            )
            .unwrap();
        assert_eq!(
            layer,
            Layer {
                layer_id: "layer:1".into(),
                block_ids: vec!["block:1".into()]
            }
        );
        let calls = fake.calls.borrow();
        assert_eq!(
            &calls[0][..5],
            &["api", "call", "-s", "session:abc", "session.new_layer"]
        );
        let sent: Value = serde_json::from_str(&calls[0][5]).unwrap();
        assert_eq!(
            sent,
            json!({
                "bounds": [0.15, 0.10, 0.70, 0.80],
                "focus": true,
                "layout": {"block": {
                    "flavor": "com.superlogical.terminal.shell",
                    "label": "axel",
                    "options": {
                        "shell": "none",
                        "command": ["/usr/bin/env", "AXEL_DB=/tmp/a.db", "axel", "ui", "launch"],
                        "cwd": "/work",
                        "exit": {"on_completion": true}
                    }
                }}
            })
        );
        assert!(
            sent.get("session_id").is_none(),
            "the CLI injects session_id from -s"
        );
    }

    #[test]
    fn new_window_payload_and_result() {
        let (rex, fake) = client(vec![Ok(
            r#"{"window_id":"window:7","block_ids":["block:9"],"revision":1}"#.into(),
        )]);
        let w = rex
            .new_window("atlas · Ship it", &["claude".into()], None, &[], false)
            .unwrap();
        assert_eq!(
            w,
            Window {
                window_id: "window:7".into(),
                block_ids: vec!["block:9".into()]
            }
        );
        let calls = fake.calls.borrow();
        assert_eq!(calls[0][4], "session.new_window");
        let sent: Value = serde_json::from_str(&calls[0][5]).unwrap();
        assert_eq!(
            sent,
            json!({
                "window_label": "atlas · Ship it",
                "focus": false,
                "layout": {"block": {
                    "flavor": "com.superlogical.terminal.shell",
                    "label": "atlas · Ship it",
                    "options": {"shell": "none", "command": ["claude"], "exit": {"on_completion": true}}
                }}
            })
        );
    }

    #[test]
    fn detached_terminal_payload_and_result() {
        let (rex, fake) = client(vec![Ok(r#"{"block_id":"block:d","revision":2}"#.into())]);
        assert_eq!(rex.new_detached_terminal("atlas · log", &["host".into()], None, &[]).unwrap(), "block:d");
        let calls = fake.calls.borrow();
        assert_eq!(calls[0][4], "session.new_block");
        let sent: Value = serde_json::from_str(&calls[0][5]).unwrap();
        assert_eq!(
            sent,
            json!({ "flavor": "com.superlogical.terminal.shell", "label": "atlas · log", "options": { "shell": "none", "command": ["host"], "exit": { "on_completion": true } } })
        );
    }

    #[test]
    fn a_composer_and_the_pane_beside_it_are_one_row() {
        let (rex, fake) = client(vec![Ok(r#"{"anchor_id":"block:s","block_ids":["block:c","block:u"],"revision":3}"#.into())]);
        let usage = vec!["usage".to_string()];
        let beside = Beside { label: "tokens", command: &usage, keep: 0.7 };
        let ids = rex.split_below_beside("block:s", "compose", &["c".into()], Some(&beside), 0.85, None, &[], true).unwrap();
        assert_eq!(ids, vec!["block:c".to_string(), "block:u".to_string()]);
        let sent: Value = serde_json::from_str(&fake.calls.borrow()[0][5]).unwrap();
        assert_eq!(sent["direction"], "vertical");
        assert_eq!(sent["layout"]["split"]["direction"], "horizontal");
        assert_eq!(sent["layout"]["split"]["ratio"], 0.7);
        assert_eq!(sent["layout"]["split"]["before"]["block"]["label"], "compose");
        assert_eq!(sent["layout"]["split"]["after"]["block"]["label"], "tokens");
    }

    #[test]
    fn split_right_stacked_payload_and_result() {
        let (rex, fake) = client(vec![Ok(r#"{"anchor_id":"split:1","block_ids":["block:a","block:b"],"revision":3}"#.into())]);
        let ids = rex
            .split_right_stacked("split:1", ("Ship it", &["s".into()]), ("compose", &["c".into()]), None, 0.4, 0.85, None, &[], true)
            .unwrap();
        assert_eq!(ids, vec!["block:a".to_string(), "block:b".to_string()]);
        let calls = fake.calls.borrow();
        assert_eq!(calls[0][4], "session.new_split");
        let sent: Value = serde_json::from_str(&calls[0][5]).unwrap();
        let block = |label: &str, cmd: &str| json!({"block": {
            "flavor": "com.superlogical.terminal.shell",
            "label": label,
            "options": {"shell": "none", "command": [cmd], "exit": {"on_completion": true}}
        }});
        assert_eq!(
            sent,
            json!({
                "anchor_id": "split:1",
                "direction": "horizontal",
                "side": "after",
                "ratio": 0.4,
                "focus": true,
                "layout": {"split": {"direction": "vertical", "before": block("Ship it", "s"), "after": block("compose", "c"), "ratio": 0.85}}
            })
        );
    }

    #[test]
    fn missing_layer_id_is_shape_error() {
        let (rex, _) = client(vec![Ok(r#"{"block_ids":[],"revision":1}"#.into())]);
        let err = rex
            .new_floating_terminal("x", [0.0, 0.0, 1.0, 1.0], &["sh".into()], None, &[])
            .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Shape {
                    field: "layer_id",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn simple_methods_send_expected_payloads() {
        let (rex, fake) = client(vec![]);
        rex.close_layer("layer:1").unwrap();
        rex.set_block_label("block:1", "  working ").unwrap();
        rex.set_window_label("window:1", "editor").unwrap();
        rex.focus_window("window:1").unwrap();
        rex.view().unwrap();
        rex.program_status("block:1").unwrap();
        let calls = fake.calls.borrow();
        let sent = |i: usize| -> (String, Value) {
            (
                calls[i][4].clone(),
                serde_json::from_str(&calls[i][5]).unwrap(),
            )
        };
        assert_eq!(
            sent(0),
            ("session.close_layer".into(), json!({"layer_id": "layer:1"}))
        );
        assert_eq!(
            sent(1),
            (
                "session.set_block_label".into(),
                json!({"block_id": "block:1", "label": "working"})
            )
        );
        assert_eq!(
            sent(2),
            (
                "session.set_window_label".into(),
                json!({"window_id": "window:1", "label": "editor"})
            )
        );
        assert_eq!(
            sent(3),
            (
                "session.focus_window".into(),
                json!({"window_id": "window:1"})
            )
        );
        assert_eq!(sent(4), ("session.view".into(), json!({})));
        assert_eq!(
            sent(5),
            (
                "com.superlogical.terminal.program_status".into(),
                json!({"block_id": "block:1", "args": {}})
            )
        );
    }

    #[test]
    fn method_of_finds_method_after_options() {
        let args: Vec<String> = ["api", "call", "-s", "session:1", "session.view", "{}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(CliRunner::method_of(&args), "session.view");
        let args: Vec<String> = ["api", "call", "server.status", "{}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(CliRunner::method_of(&args), "server.status");
    }

    #[test]
    fn cli_runner_returns_stdout_and_reports_stderr_on_failure() {
        let runner = CliRunner { program: "/bin/sh".into(), server: None };
        let ok = runner.run(&["-c".into(), "echo out; echo noise >&2".into()]).unwrap();
        assert_eq!(ok, "out\n");
        match runner.run(&["-c".into(), "echo bad >&2; exit 3".into()]) {
            Err(Error::Rex { stderr, .. }) => assert_eq!(stderr.trim(), "bad"),
            other => panic!("{other:?}"),
        }
        match runner.run(&["-c".into(), "exit 4".into()]) {
            Err(Error::Rex { stderr, .. }) => assert!(stderr.contains("exit status"), "{stderr}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cli_runner_reports_spawn_failure() {
        let runner = CliRunner {
            program: "/nonexistent/axel-rex-test-binary".into(),
            server: None,
        };
        match runner.run(&["api".into(), "call".into(), "server.status".into()]) {
            Err(Error::Spawn { program, .. }) => assert!(program.contains("nonexistent")),
            other => panic!("{other:?}"),
        }
    }
}
