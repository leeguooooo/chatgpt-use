//! Append-only event ledger at `~/.chatgpt-use/ledger.jsonl` — a best-effort
//! audit trail of what was asked, delegated, and handed off. Borrowed from
//! cccc's single-source-of-truth ledger idea (simplified: one JSON line per
//! event, `{v, ts, kind, data}`). Writing never blocks or fails the task — a
//! ledger error only warns.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Append one event to the ledger (best-effort; warns on failure).
pub fn record(kind: &str, data: Value) {
    if let Err(e) = record_to(&ledger_dir(), kind, data) {
        eprintln!("warning: ledger write failed: {e}");
    }
}

/// `~/.chatgpt-use` (falls back to the current dir if HOME is unset).
fn ledger_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".chatgpt-use")
}

/// Testable core: append the event line under `dir/ledger.jsonl`.
fn record_to(dir: &Path, kind: &str, data: Value) -> std::io::Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::fs::create_dir_all(dir)?;
    let line = json!({ "v": 1, "ts": ts, "kind": kind, "data": data });
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ledger.jsonl"))?;
    writeln!(f, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_one_json_line_per_event() {
        let dir = std::env::temp_dir().join(format!("cgu-ledger-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        record_to(&dir, "ask", json!({"prompt": "hi"})).unwrap();
        record_to(&dir, "handoff", json!({"to": "codex", "executed": false})).unwrap();

        let body = std::fs::read_to_string(dir.join("ledger.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["kind"], "ask");
        assert_eq!(first["v"], 1);
        assert!(first["ts"].as_u64().is_some());
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["kind"], "handoff");
        assert_eq!(second["data"]["executed"], false);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
