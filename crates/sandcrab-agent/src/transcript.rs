//! JSONL transcript: one JSON object per line (session, requests, responses, tool calls).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};

#[derive(Debug)]
pub struct Transcript {
    path: PathBuf,
    out: BufWriter<File>,
    start: Instant,
}

/// `<state root>/transcripts/<session>.jsonl`.
#[must_use]
pub fn default_path(state_root: &Path, session: &str) -> PathBuf {
    state_root
        .join("transcripts")
        .join(format!("{session}.jsonl"))
}

/// `20261123-145501-1a2b3c`.
#[must_use]
pub fn new_session_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!(
        "{}-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        &id[..6]
    )
}

impl Transcript {
    /// Create (append to) the file, parent directories included; owner-only permissions.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        Ok(Self {
            path: path.to_owned(),
            out: BufWriter::new(opts.open(path)?),
            start: Instant::now(),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append `{"type": kind, "ts", "t_ms", ...fields}`. Write errors are reported once
    /// to stderr and otherwise ignored (the transcript must never break the session).
    pub fn log(&mut self, kind: &str, fields: Value) {
        let mut v = json!({
            "type": kind,
            "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "t_ms": self.start.elapsed().as_millis() as u64,
        });
        if let (Some(o), Value::Object(f)) = (v.as_object_mut(), fields) {
            o.extend(f);
        }
        let res = serde_json::to_writer(&mut self.out, &v)
            .map_err(io::Error::from)
            .and_then(|()| self.out.write_all(b"\n"))
            .and_then(|()| self.out.flush());
        if let Err(e) = res {
            tracing::warn!("transcript write to {} failed: {e}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let p = default_path(dir.path(), "s1");
        let mut t = Transcript::open(&p).unwrap();
        t.log("a", json!({"x": 1}));
        t.log("b", json!({"y": "two"}));
        let text = fs::read_to_string(&p).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["type"], "a");
        assert_eq!(lines[0]["x"], 1);
        assert_eq!(lines[1]["y"], "two");
        assert!(lines[1]["ts"].as_str().unwrap().ends_with('Z'));
        assert!(new_session_id().len() > 10);
    }
}
