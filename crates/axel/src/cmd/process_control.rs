//! Suspend a session host and its descendants without discarding its memory.
//! The controlling dashboard stays outside that process tree, so it can resume
//! the session even when its socket server and adapter are stopped.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use axel_store::{Session, SessionState, SessionUpdate};
use serde::{Deserialize, Serialize};

use super::Ctx;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Identity { pid: i32, started: String }

struct Process { identity: Identity, parent: i32, command: Vec<String> }

#[derive(Serialize, Deserialize)]
struct Suspension { session_id: String, previous: SessionState, processes: Vec<Identity> }

fn processes() -> Result<HashMap<i32, Process>> {
    let output = Command::new("ps").args(["-axo", "pid=,ppid=,lstart=,command="]).output().context("listing session processes")?;
    if !output.status.success() { bail!("could not list session processes") }
    let mut out = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 8 { continue }
        let (Ok(pid), Ok(parent)) = (fields[0].parse(), fields[1].parse()) else { continue };
        out.insert(pid, Process { identity: Identity { pid, started: fields[2..7].join(" ") }, parent, command: fields[7..].iter().map(|s| s.to_string()).collect() });
    }
    Ok(out)
}

fn signal(identity: &Identity, signal: i32) -> Result<bool> {
    // Never signal a PID that has since been reused by an unrelated process.
    if !processes()?.get(&identity.pid).is_some_and(|p| p.identity.started == identity.started) { return Ok(false) }
    // SAFETY: this identity was freshly checked against the process table.
    if unsafe { libc::kill(identity.pid, signal) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) { return Ok(false) }
        return Err(error.into());
    }
    Ok(true)
}

fn continue_all(identities: &[Identity]) -> Result<()> {
    for identity in identities.iter().rev() { signal(identity, libc::SIGCONT)?; }
    Ok(())
}

fn suspend(roots: Vec<Identity>, mut record: impl FnMut(&[Identity]) -> Result<()>) -> Result<Vec<Identity>> {
    let mut stopped = Vec::new();
    let result = (|| -> Result<()> {
        for root in roots { stopped.push(root); record(&stopped)?; if !signal(stopped.last().unwrap(), libc::SIGSTOP)? { bail!("session exited before it could be paused") } }
        // Freeze parents before discovering children. Rescan after freezing so
        // a child spawned during the previous snapshot cannot escape the pause.
        loop {
            let current = processes()?;
            let known: HashSet<_> = stopped.iter().filter(|p| current.get(&p.pid).is_some_and(|live| live.identity.started == p.started)).map(|p| p.pid).collect();
            let children: Vec<_> = current.into_values().filter(|p| known.contains(&p.parent) && !known.contains(&p.identity.pid)).map(|p| p.identity).collect();
            if children.is_empty() { return Ok(()) }
            for child in children { stopped.push(child); record(&stopped)?; signal(stopped.last().unwrap(), libc::SIGSTOP)?; }
        }
    })();
    if let Err(error) = result {
        let _ = continue_all(&stopped);
        return Err(error);
    }
    Ok(stopped)
}

fn path(session: &Session) -> PathBuf {
    crate::paths::home().join("run").join(format!("{}.paused.json", session.id))
}

pub(super) fn pause(ctx: &Ctx, session: &Session) -> Result<()> {
    if session.state == SessionState::Paused { return Ok(()) }
    if session.ended_at.is_some() { bail!("this session has ended") }
    let mut roots: Vec<_> = processes()?.into_values().filter(|p| {
        p.command.first().is_some_and(|s| s.rsplit('/').next() == Some("axel"))
            && p.command.get(1).map(String::as_str) == Some("session")
            && p.command.get(2).map(String::as_str) == Some("host")
            && p.command.get(3) == Some(&session.id)
    }).map(|p| p.identity).collect();
    if roots.is_empty() && session.runtime == axel_store::SessionRuntime::Pane {
        if let (Some(rex), Some(block)) = (&ctx.rex, &session.rex_block_id) {
            let info = rex.process(block)?;
            if let Some(pid) = info.pointer("/child/pid").and_then(|v| v.as_i64()) {
                if let Some(process) = processes()?.remove(&(pid as i32)) { roots.push(process.identity); }
            }
        }
    }
    if roots.is_empty() { bail!("this session's process is not running here") }
    let signer = ctx.agent_signer(&session.agent_pubkey, Some(&session.thread_id)).or_else(|_| ctx.owner())?;
    let file = path(session);
    std::fs::create_dir_all(file.parent().unwrap())?;
    let record = |identities: &[Identity]| -> Result<()> {
        let snapshot = Suspension { session_id: session.id.clone(), previous: session.state, processes: identities.to_vec() };
        let pending = file.with_extension("tmp");
        std::fs::write(&pending, serde_json::to_vec(&snapshot)?)?;
        std::fs::rename(pending, &file)?;
        Ok(())
    };
    // Persist recovery information before freezing anything. A dashboard exit
    // during pause still leaves Resume available with valid process identities.
    record(&roots)?;
    ctx.store.update_session(&signer, &session.id, &SessionUpdate { state: Some(SessionState::Paused), ..Default::default() })?;
    let result = suspend(roots, record);
    match result {
        Ok(stopped) => {
            if let Err(error) = ctx.store.update_session(&signer, &session.id, &SessionUpdate { state: Some(SessionState::Paused), ..Default::default() }) {
                let _ = continue_all(&stopped);
                let _ = ctx.store.update_session(&signer, &session.id, &SessionUpdate { state: Some(session.state), ..Default::default() });
                let _ = std::fs::remove_file(&file);
                return Err(error.into());
            }
        }
        Err(error) => {
            let _ = ctx.store.update_session(&signer, &session.id, &SessionUpdate { state: Some(session.state), ..Default::default() });
            let _ = std::fs::remove_file(&file);
            return Err(error);
        }
    }
    Ok(())
}

pub(super) fn resume(ctx: &Ctx, session: &Session) -> Result<()> {
    if session.state != SessionState::Paused { return Ok(()) }
    let snapshot: Suspension = serde_json::from_slice(&std::fs::read(path(session)).context("reading the paused session's process record")?)?;
    if snapshot.session_id != session.id { bail!("paused process record belongs to another session") }
    // Restore the store before waking old hosts, which may use an older enum
    // and will then keep reporting their original in-memory lifecycle state.
    let signer = ctx.agent_signer(&session.agent_pubkey, Some(&session.thread_id)).or_else(|_| ctx.owner())?;
    ctx.store.update_session(&signer, &session.id, &SessionUpdate { state: Some(snapshot.previous), ..Default::default() })?;
    if let Err(error) = continue_all(&snapshot.processes) {
        let _ = ctx.store.update_session(&signer, &session.id, &SessionUpdate { state: Some(SessionState::Paused), ..Default::default() });
        return Err(error);
    }
    std::fs::remove_file(path(session))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn suspends_and_resumes_an_isolated_process_tree() {
        let mut child = Command::new("/bin/sh").args(["-c", "sleep 30 & wait"]).process_group(0).spawn().unwrap();
        struct Cleanup(i32);
        impl Drop for Cleanup { fn drop(&mut self) { if self.0 > 0 { unsafe { libc::kill(-self.0, libc::SIGKILL); } } } }
        let mut cleanup = Cleanup(child.id() as i32);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let root = processes().unwrap().remove(&(child.id() as i32)).unwrap().identity;
        let mut writes = 0;
        assert!(suspend(vec![root.clone()], |_| { writes += 1; if writes > 1 { bail!("simulated recovery-record failure") } Ok(()) }).is_err());
        let root_state = String::from_utf8(Command::new("ps").args(["-o", "stat=", "-p", &root.pid.to_string()]).output().unwrap().stdout).unwrap();
        assert!(!root_state.contains('T'), "a failed pause must wake the processes it already stopped");
        let stopped = suspend(vec![root], |_| Ok(())).unwrap();
        assert!(stopped.len() >= 2, "the tool process is suspended with its parent");
        let state = |pid: i32| String::from_utf8(Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().unwrap().stdout).unwrap();
        assert!(stopped.iter().all(|p| state(p.pid).contains('T')));
        continue_all(&stopped).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(stopped.iter().all(|p| !state(p.pid).contains('T')));
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL); }
        child.wait().unwrap();
        cleanup.0 = 0;
    }
}
