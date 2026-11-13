//! Spawning the adapter and wiring its stdio into the SDK as a line transport.
//!
//! The SDK's own `AcpAgent` cannot set the child's working directory and
//! gives no hook for raw frames, so the host spawns the adapter itself and
//! hands the SDK a [`Lines`] transport over the child's pipes.  Every line
//! that passes is appended to the [`FrameLog`].

use std::io;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use agent_client_protocol::{Client, ConnectTo, Lines};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::UnboundedSender;

use crate::log::{Dir, FrameLog};

/// Spawn `cmd` in `cwd` with extra `env`; return the child and a transport
/// speaking NDJSON over its stdin/stdout.  Stderr lines go to `stderr_tx`.
pub fn spawn(
    cmd: &[String],
    cwd: &Path,
    env: &[(String, String)],
    log: Arc<FrameLog>,
    stderr_tx: UnboundedSender<String>,
) -> io::Result<(Child, impl ConnectTo<Client>)> {
    let (program, args) = cmd
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty adapter command"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Own process group so killing the adapter also reaches wrappers (`npx …`).
    command.process_group(0);
    let mut child = command.spawn().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("failed to spawn adapter `{program}`: {e}"),
        )
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("no stderr"))?;

    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if stderr_tx.send(line).is_err() {
                break;
            }
        }
    });

    let log_in = log.clone();
    let incoming = futures::stream::unfold(BufReader::new(stdout).lines(), move |mut lines| {
        let log = log_in.clone();
        async move {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    log.record(Dir::In, &line);
                    Some((Ok(line), lines))
                }
                Ok(None) => None,
                Err(e) => Some((Err(e), lines)),
            }
        }
    });

    let outgoing = futures::sink::unfold(stdin, move |mut w, line: String| {
        let log = log.clone();
        async move {
            log.record(Dir::Out, &line);
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            w.write_all(&bytes).await?;
            w.flush().await?;
            Ok::<_, io::Error>(w)
        }
    });

    Ok((child, Lines::new(outgoing, incoming)))
}
