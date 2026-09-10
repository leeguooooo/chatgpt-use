//! Request receipts for `ask --request-id`: a durable record of one request,
//! written before anything is sent and updated until the turn ends, so a
//! caller that lost track of it (a timeout, a crash, a kill) can ask what
//! happened instead of sending the prompt again.
//!
//! One JSON file per id at `~/.chatgpt-use/requests/<id>.json`, replaced
//! atomically. The rule it enforces: an id whose request may have reached
//! ChatGPT is never sent again. Only a receipt that proves nothing was
//! submitted frees its id for a retry.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Receipt {
    pub request_id: String,
    /// accepted → submitted → completed | failed
    pub state: String,
    /// Whether the prompt reached ChatGPT: no | yes | unknown.
    pub submitted: String,
    pub conversation_id: Option<String>,
    /// The process that owns the request while it runs.
    pub pid: u32,
    pub created_at: u64,
    pub updated_at: u64,
    /// The result envelope's status once the turn ended (completed, incomplete, …).
    pub outcome: Option<String>,
    pub error: Option<String>,
}

impl Receipt {
    pub fn accepted(id: &str) -> Self {
        let now = now();
        Receipt {
            request_id: id.to_string(),
            state: "accepted".into(),
            submitted: "no".into(),
            conversation_id: None,
            pid: std::process::id(),
            created_at: now,
            updated_at: now,
            outcome: None,
            error: None,
        }
    }
}

/// Ids become file names, so only a conservative alphabet is accepted.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn path_for(id: &str) -> PathBuf {
    crate::ledger::ledger_dir().join("requests").join(format!("{id}.json"))
}

pub fn load(path: &Path) -> Option<Receipt> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Write via a temp file and rename, so a reader never sees half a receipt.
pub fn save(path: &Path, receipt: &Receipt) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(serde_json::to_string_pretty(receipt)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Claim a new id: create its receipt only if none exists. Returns false if
/// one already does, so two callers racing on the same id cannot both win.
pub fn create(path: &Path, receipt: &Receipt) -> std::io::Result<bool> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            f.write_all(serde_json::to_string_pretty(receipt)?.as_bytes())?;
            f.sync_all()?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Load, change, save. Best-effort: a receipt that cannot be updated must not
/// fail the turn it describes, so the failure is only reported.
pub fn update(path: &Path, change: impl FnOnce(&mut Receipt)) {
    let Some(mut r) = load(path) else {
        eprintln!("warning: receipt {} is missing; not updated", path.display());
        return;
    };
    change(&mut r);
    r.updated_at = now();
    if let Err(e) = save(path, &r) {
        eprintln!("warning: could not update receipt {}: {e}", path.display());
    }
}

/// Close the receipt with the turn's outcome. A reply that exists (even one
/// that failed validation) means the turn completed and the prompt was sent;
/// otherwise the failure says how far it got.
pub fn finish(path: &Path, envelope: &serde_json::Value) {
    let status = envelope["status"].as_str().unwrap_or("failed").to_string();
    let replied = matches!(status.as_str(), "completed" | "schema_violation" | "unparseable");
    let submitted = if replied {
        "yes".to_string()
    } else {
        envelope["error"]["submitted"].as_str().unwrap_or("unknown").to_string()
    };
    let error = envelope["error"]["message"].as_str().map(str::to_string);
    let convo = envelope["conversation_id"].as_str().map(str::to_string);
    update(path, |r| {
        r.state = if replied { "completed" } else { "failed" }.into();
        // Never downgrade: the channel may already have recorded the prompt
        // as sent, which a later "not submitted" cannot undo.
        if !(r.submitted == "yes" && submitted != "yes") {
            r.submitted = submitted;
        }
        r.outcome = Some(status);
        r.error = error;
        if convo.is_some() {
            r.conversation_id = convo;
        }
    });
}

/// Whether a request may be sent under an id that already has this receipt.
pub fn may_send(existing: Option<&Receipt>) -> bool {
    match existing {
        None => true,
        Some(r) => r.state == "failed" && r.submitted == "no",
    }
}

/// The state `status` reports. A receipt still marked in-flight is resolved
/// by whether its owner is alive: a dead owner that never recorded a
/// submission may still have pressed Enter, which is `submission_unknown` —
/// not permission to resend.
pub fn live_state(r: &Receipt, alive: impl Fn(u32) -> bool) -> String {
    match r.state.as_str() {
        "accepted" | "submitted" if alive(r.pid) => "running".into(),
        "accepted" => "submission_unknown".into(),
        "submitted" => "detached".into(),
        other => other.into(),
    }
}

pub fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_restricted_to_a_file_safe_alphabet() {
        for ok in ["pr-42", "gogs.review_7", "A1"] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in ["", "../x", ".hidden", "a/b", "a b", &"x".repeat(129)] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn only_a_proven_unsent_request_frees_its_id() {
        let mut r = Receipt::accepted("x");
        assert!(may_send(None));
        for (state, submitted, ok) in [
            ("accepted", "no", false),
            ("submitted", "yes", false),
            ("completed", "yes", false),
            ("failed", "yes", false),
            ("failed", "unknown", false),
            ("failed", "no", true),
        ] {
            r.state = state.into();
            r.submitted = submitted.into();
            assert_eq!(may_send(Some(&r)), ok, "{state}/{submitted}");
        }
    }

    #[test]
    fn a_dead_owner_is_never_read_as_not_sent() {
        let mut r = Receipt::accepted("x");
        assert_eq!(live_state(&r, |_| true), "running");
        assert_eq!(live_state(&r, |_| false), "submission_unknown");
        r.state = "submitted".into();
        assert_eq!(live_state(&r, |_| false), "detached");
        r.state = "completed".into();
        assert_eq!(live_state(&r, |_| false), "completed");
    }

    #[test]
    fn save_is_atomic_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("cgu-receipt-{}", std::process::id()));
        let path = dir.join("r.json");
        let r = Receipt::accepted("r");
        assert!(create(&path, &r).unwrap());
        assert_eq!(load(&path), Some(r.clone()));
        assert!(!create(&path, &r).unwrap(), "a second claim on the same id must lose");
        update(&path, |r| r.state = "submitted".into());
        assert_eq!(load(&path).unwrap().state, "submitted");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
