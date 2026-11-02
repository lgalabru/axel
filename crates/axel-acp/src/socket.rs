//! The host's command channel: JSON lines over a unix socket.
//!
//! The thread view and the CLI talk to a running `axel session host` through
//! `$AXEL_HOME/run/SESSION.sock`.  Each connection sends one [`Command`] as a
//! JSON line and reads one [`Reply`] JSON line back.  [`send`] is the
//! synchronous client used by the TUI and CLI; the server side lives in the
//! host and is driven by its event loop.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axel_store::SessionState;
use serde::{Deserialize, Serialize};

/// How long [`send`] waits to connect, write and read.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// What a client can ask the host to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Start a turn with this user text (`session/prompt`), optionally with
    /// file attachments (absolute paths preferred).  While a turn is running
    /// the prompt is queued (replied with [`Reply::Queued`]) and starts when
    /// the turn ends; `steer` puts it first and cancels the running turn.
    Prompt {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<PathBuf>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        steer: bool,
        /// Another agent wrote this (`axel send`), not the owner.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Sender>,
    },
    /// A note from another agent that needs no reply: it shows on the thread
    /// and the agent reads it with its next turn, but it starts no turn.
    Note { text: String, from: Sender },
    /// Send a queued prompt now: move it first and cancel the running turn.
    Steer { item_id: String },
    /// Withdraw a queued prompt.
    Dequeue { item_id: String },
    /// Switch the session's runtime mode (policy and the adapter's native mode).
    SetMode { mode: axel_store::RuntimeMode },
    /// Cancel the running turn (`session/cancel`); also rejects pending permissions.
    Cancel,
    /// Answer a pending `Permission` item by option id.
    Answer { item_id: String, option_id: String },
    /// Report the session's state.
    Status,
    /// End the session and exit the host.
    Stop,
}

/// The host's one-line answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Ok,
    /// The prompt was queued behind the running turn as this `UserMessage`
    /// item (status `pending` until its turn starts).
    Queued {
        item_id: String,
    },
    Status {
        state: SessionState,
        turn: i64,
        pending: usize,
        #[serde(default)]
        queued: usize,
    },
    Error {
        message: String,
    },
}

/// Send one command and wait for its reply (blocking, [`TIMEOUT`]).
pub fn send(socket_path: &Path, cmd: &Command) -> std::io::Result<Reply> {
    let stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut line = serde_json::to_string(cmd).map_err(std::io::Error::other)?;
    line.push('\n');
    (&stream).write_all(line.as_bytes())?;
    (&stream).flush()?;
    let mut reader = BufReader::new(&stream);
    let mut reply = String::new();
    let n = reader.read_line(&mut reply)?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "host closed the socket without replying",
        ));
    }
    serde_json::from_str(reply.trim()).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn notes_round_trip_with_their_sender() {
        let note = Command::Note { text: "FYI: green".into(), from: Sender { pubkey: "PK".into(), name: "nova".into() } };
        let v = serde_json::to_value(&note).unwrap();
        assert_eq!(v, serde_json::json!({ "cmd": "note", "text": "FYI: green", "from": { "pubkey": "PK", "name": "nova" } }));
        assert_eq!(serde_json::from_value::<Command>(v).unwrap(), note);
    }

    #[test]
    fn commands_and_replies_are_tagged_json() {
        let c = serde_json::to_value(Command::Answer {
            item_id: "i".into(),
            option_id: "o".into(),
        })
        .unwrap();
        assert_eq!(
            c,
            serde_json::json!({"cmd": "answer", "item_id": "i", "option_id": "o"})
        );
        let r: Reply =
            serde_json::from_str(r#"{"reply":"status","state":"idle","turn":2,"pending":0}"#)
                .unwrap();
        assert_eq!(
            r,
            Reply::Status {
                state: SessionState::Idle,
                turn: 2,
                pending: 0,
                queued: 0,
            }
        );
        // `steer` is omitted when false so old hosts still parse plain prompts.
        assert_eq!(
            serde_json::to_value(Command::Prompt { text: "x".into(), attachments: vec![], steer: false, from: None }).unwrap(),
            serde_json::json!({"cmd": "prompt", "text": "x"})
        );
        let c: Command = serde_json::from_str(r#"{"cmd":"prompt","text":"x","steer":true}"#).unwrap();
        assert_eq!(c, Command::Prompt { text: "x".into(), attachments: vec![], steer: true, from: None });
        assert_eq!(
            serde_json::to_string(&Reply::Ok).unwrap(),
            r#"{"reply":"ok"}"#
        );
    }

    #[test]
    fn round_trip_over_a_listener() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let cmd: Command = serde_json::from_str(line.trim()).unwrap();
            let reply = match cmd {
                Command::Prompt { text, attachments, steer: false, from: None } if text == "hi" && attachments.is_empty() => {
                    Reply::Ok
                }
                Command::Status => Reply::Status {
                    state: SessionState::Running,
                    turn: 1,
                    pending: 1,
                    queued: 0,
                },
                other => Reply::Error {
                    message: format!("unexpected {other:?}"),
                },
            };
            let mut out = serde_json::to_string(&reply).unwrap();
            out.push('\n');
            (&stream).write_all(out.as_bytes()).unwrap();
        });
        let reply = send(
            &path,
            &Command::Prompt {
                text: "hi".into(),
                attachments: vec![],
                steer: false,
                from: None,
            },
        )
        .unwrap();
        assert_eq!(reply, Reply::Ok);
        server.join().unwrap();
    }

    #[test]
    fn missing_socket_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(send(&dir.path().join("nope.sock"), &Command::Status).is_err());
    }
}

/// The agent a prompt comes from, when it isn't the owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    pub pubkey: String,
    pub name: String,
}

impl Sender {
    /// What the receiving agent reads: who wrote, and how to answer.  Its
    /// replies on the thread reach the owner only, not the sender.
    pub fn frame(&self, text: &str) -> String {
        format!(
            "Message from {name}, another Axel agent on this thread:\n\n{text}\n\n\
             ---\nOnly the owner sees your replies here. To answer {name}, use the axel `send` tool with to: [\"{name}\"] \
             (or run `axel send \"@{name} <your reply>\"`) only when you have something {name} needs: \
             a result, an answer, or a question. Don't send \"received\", \"thanks\" or \"on it\": \
             every message costs {name} a turn.",
            name = self.name
        )
    }
}
