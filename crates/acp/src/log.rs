//! Raw frame log: every JSON-RPC line in or out, appended to `~/.axel/logs/SESSION.jsonl`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// Direction of a frame relative to the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Agent → host.
    In,
    /// Host → agent.
    Out,
}

impl Dir {
    fn as_str(self) -> &'static str {
        match self {
            Dir::In => "in",
            Dir::Out => "out",
        }
    }
}

/// An append-only `{"ts":ms,"dir":"in"|"out","frame":<json>}` log.
pub struct FrameLog {
    file: Mutex<File>,
}

impl FrameLog {
    /// Open (append) `path`, creating parent directories.
    pub fn open(path: &Path) -> std::io::Result<FrameLog> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(FrameLog {
            file: Mutex::new(file),
        })
    }

    /// Append one frame.  `frame` is the raw line; if it is not valid JSON it
    /// is logged as a string.  Errors are swallowed: logging never stops the host.
    pub fn record(&self, dir: Dir, frame: &str) {
        let payload = match serde_json::from_str::<&serde_json::value::RawValue>(frame) {
            Ok(_) => frame.to_string(),
            Err(_) => serde_json::Value::String(frame.to_string()).to_string(),
        };
        let line = format!(
            "{{\"ts\":{},\"dir\":\"{}\",\"frame\":{}}}\n",
            now_ms(),
            dir.as_str(),
            payload
        );
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Unix time in milliseconds.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_json_lines_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/s.jsonl");
        let log = FrameLog::open(&path).unwrap();
        log.record(
            Dir::Out,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        );
        log.record(Dir::In, "not json");
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["dir"], "out");
        assert_eq!(lines[0]["frame"]["method"], "initialize");
        assert!(lines[0]["ts"].as_i64().unwrap() > 0);
        assert_eq!(lines[1]["frame"], "not json");
    }
}
