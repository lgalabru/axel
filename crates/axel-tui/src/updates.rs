//! A background-only release check. The composer only drains a channel;
//! it never waits for a network request or cache read.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CACHE_SECS: u64 = 6 * 60 * 60;
const RETRY: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let values: Vec<_> = text.trim().trim_start_matches('v').split('.').map(str::parse::<u64>).collect::<Result<_, _>>().ok()?;
        match values.as_slice() { [major, minor, patch] => Some(Self(*major, *minor, *patch)), _ => None }
    }

    fn worth_suggesting(self, installed: Self) -> bool {
        self > installed && (self.0 > installed.0 || self.1 > installed.1 || self.2.saturating_sub(installed.2) > 10)
    }
}

#[derive(Serialize, Deserialize)]
struct Cache { checked_at: u64, latest: String }

fn cache_path() -> Option<PathBuf> {
    let home = std::env::var_os("AXEL_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|p| p.join(".axel")))?;
    Some(home.join("cache").join("release.json"))
}

fn fetch() -> Option<String> {
    let output = std::process::Command::new("curl").args([
        "--fail", "--silent", "--location", "--connect-timeout", "2", "--max-time", "5",
        "--header", "Accept: application/vnd.github+json", "--header", "User-Agent: axel-update-check",
        "https://api.github.com/repos/lgalabru/axel/releases/latest",
    ]).output().ok()?;
    if !output.status.success() { return None }
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let latest = body.get("tag_name")?.as_str()?.to_string();
    Version::parse(&latest)?;
    Some(latest)
}

fn cached_latest(path: &Path, now: u64, fetch: impl FnOnce() -> Option<String>) -> Option<String> {
    let cached: Option<Cache> = std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    if let Some(cache) = &cached
        && cache.checked_at <= now && now - cache.checked_at < CACHE_SECS
    { return Some(cache.latest.clone()) }
    std::fs::create_dir_all(path.parent()?).ok()?;
    let lock = path.with_extension("lock");
    if std::fs::metadata(&lock).ok().and_then(|m| m.modified().ok()).is_some_and(|t| t.elapsed().unwrap_or(Duration::MAX) > Duration::from_secs(60)) {
        let _ = std::fs::remove_file(&lock);
    }
    let Ok(_file) = std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) else {
        return cached.map(|c| c.latest);
    };
    struct Unlock(PathBuf);
    impl Drop for Unlock { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }
    let _unlock = Unlock(lock);
    let Some(latest) = fetch().filter(|tag| Version::parse(tag).is_some()) else { return cached.map(|c| c.latest) };
    let cache = Cache { checked_at: now, latest: latest.clone() };
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let pending = path.with_extension("pending");
        if std::fs::write(&pending, bytes).is_ok() { let _ = std::fs::rename(pending, path); }
    }
    Some(latest)
}

fn hint(installed: &str, latest: &str) -> Option<String> {
    Version::parse(latest)?.worth_suggesting(Version::parse(installed)?).then(|| format!("Axel {} available · axel update", latest.trim_start_matches('v')))
}

fn spawn(check: impl Fn() -> Option<String> + Send + 'static) -> Receiver<Option<String>> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("axel-updates".into()).spawn(move || loop {
        if tx.send(check()).is_err() { break }
        std::thread::sleep(RETRY);
    });
    rx
}

pub(super) fn start() -> Receiver<Option<String>> {
    spawn(|| {
        let path = cache_path()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        hint(env!("CARGO_PKG_VERSION"), &cached_latest(&path, now, fetch)?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_only_a_minor_major_or_more_than_ten_patches() {
        for (installed, latest, wanted) in [
            ("0.3.100", "v0.3.110", false), ("0.3.100", "v0.3.111", true),
            ("0.3.999", "v0.4.0", true), ("0.9.999", "v1.0.0", true),
            ("1.0.1", "v0.99.999", false), ("1.2.3", "v1.2.3", false),
            ("1.2.3", "v1.2.999-beta", false), ("dev", "v2.0.0", false),
        ] { assert_eq!(hint(installed, latest).is_some(), wanted, "{installed} → {latest}"); }
    }

    #[test]
    fn a_slow_check_never_blocks_the_composer() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let rx = spawn(move || { let _ = ready_tx.send(()); release_rx.lock().unwrap().recv().ok()?; Some("axel update".into()) });
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release_tx.send(()).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), Some("axel update".into()));
    }

    #[test]
    fn fresh_cache_skips_network_and_failure_preserves_the_last_result() {
        let dir = std::env::temp_dir().join(format!("axel-update-test-{}", axel_store::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("release.json");
        assert_eq!(cached_latest(&path, 100, || Some("v1.2.3".into())), Some("v1.2.3".into()));
        assert_eq!(cached_latest(&path, 101, || panic!("fresh cache must avoid network")), Some("v1.2.3".into()));
        assert_eq!(cached_latest(&path, 100 + CACHE_SECS, || None), Some("v1.2.3".into()));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
