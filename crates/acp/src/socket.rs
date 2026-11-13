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
    /// file attachments (absolute paths preferred).  While a turn is running,
    /// the owner's prompt goes into it, for the agent to read at its next step,
    /// when the harness can take one ([`Reply::Steering`]); otherwise, and for
    /// another agent's, it is queued ([`Reply::Queued`]) and starts when the
    /// turn ends.  `steer` instead cancels the running turn and starts this.
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
    /// Send a queued prompt now: into the running turn when the harness can
    /// take it there, else first, cancelling the running turn.
    Steer { item_id: String },
    /// Withdraw a prompt not read yet (queued, or sent into the turn and not
    /// taken in).
    Dequeue { item_id: String },
    /// Change the text of a prompt not read yet; an error once the agent has.
    Edit { item_id: String, text: String },
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
    /// The prompt went into the running turn as this `UserMessage` item
    /// (`pending` until the agent takes it in, at its next step).
    Steering {
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
    fn a_message_says_its_answer_goes_back_and_an_answer_says_not_to_repeat_it() {
        let ask = Sender::new("PKatlas", "atlas").frame("do you know their birth date?");
        assert!(ask.starts_with("Message from atlas, another Axel agent on this thread:\n\ndo you know their birth date?"));
        assert!(ask.contains("your answer goes back to atlas by itself") && ask.contains("Don't `send` it to atlas as well"), "{ask}");
        let back = Sender { reply: true, ..Sender::new("PKorion", "orion") }.frame("No, I don't.");
        assert!(back.starts_with("orion's answer to your message:\n\nNo, I don't."));
        assert!(back.contains("The owner has already seen this answer") && back.contains("don't repeat or summarise it"), "{back}");
        // Old hosts never sent `reply`: it defaults to a message.
        let old: Sender = serde_json::from_value(serde_json::json!({ "pubkey": "PK", "name": "nova" })).unwrap();
        assert!(!old.reply);
        assert_eq!(serde_json::to_value(Sender::new("PK", "nova")).unwrap(), serde_json::json!({ "pubkey": "PK", "name": "nova" }));
    }

    #[test]
    fn notes_round_trip_with_their_sender() {
        let note = Command::Note { text: "FYI: green".into(), from: Sender::new("PK", "nova") };
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
    /// It is `name`'s answer to a message from the recipient, returned on its
    /// own when `name`'s turn ended (see [`Sender::frame`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reply: bool,
}

impl Sender {
    /// An agent's message (not a reply).
    pub fn new(pubkey: impl Into<String>, name: impl Into<String>) -> Sender {
        Sender { pubkey: pubkey.into(), name: name.into(), reply: false }
    }

    /// What the receiving agent reads: who wrote, and what happens to its answer.
    ///
    /// A message's answer goes back to its sender by itself (the host returns
    /// the turn's reply), so the recipient just answers. A returned answer the
    /// owner has already seen: the asker uses it, without repeating it.
    pub fn frame(&self, text: &str) -> String {
        let name = &self.name;
        if self.reply {
            format!(
                "{name}'s answer to your message:\n\n{text}\n\n\
                 ---\nThe owner has already seen this answer on the thread. Use it to carry on with your task; \
                 don't repeat or summarise it for the owner. If it settles what you were doing, end your turn with \
                 a line at most. Don't reply to {name} unless you need something more from it."
            )
        } else {
            format!(
                "Message from {name}, another Axel agent on this thread:\n\n{text}\n\n\
                 ---\nJust answer here: when your turn ends, your answer goes back to {name} by itself, and the owner \
                 sees it on the thread. Don't `send` it to {name} as well, and don't announce what you're about to do \
                 (\"I'll check…\", \"replying to {name}\"): give the answer."
            )
        }
    }
}
