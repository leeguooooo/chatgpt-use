//! `status <request-id>` — report what happened to an `ask --request-id`
//! request, from its receipt alone. It never touches the browser, so it
//! answers even while another run holds the ChatGPT window.

use crate::cli::StatusArgs;
use crate::receipt;
use anyhow::{bail, Result};

pub fn run(args: &StatusArgs) -> Result<()> {
    let envelope = envelope(&args.request_id);
    if envelope.get("request_id").is_none() || envelope.get("state").is_none() {
        bail!("{}", envelope["error"]["message"].as_str().unwrap_or("status failed"));
    }
    println!("{envelope}");
    Ok(())
}

/// The status of request `id`, from its receipt alone: the receipt's fields,
/// or a failure envelope when there is no readable receipt.
pub(crate) fn envelope(id: &str) -> serde_json::Value {
    let failure = |kind: &str, message: String| {
        serde_json::json!({"status": "failed", "error": {"kind": kind, "message": message, "submitted": "no"}})
    };
    if !receipt::valid_id(id) {
        return failure("error", format!("invalid request id {id:?}"));
    }
    let Some(r) = receipt::load(&receipt::path_for(id)) else {
        return failure("unknown_request", format!("no receipt for request {id:?}"));
    };
    let state = receipt::live_state(&r, receipt::pid_alive);
    // The receipt's "no" only meant "not recorded yet" while the owner ran; an
    // owner that died before recording anything may still have pressed Enter.
    let submitted = if state == "submission_unknown" { "unknown" } else { r.submitted.as_str() };
    serde_json::json!({
        "request_id": r.request_id,
        "state": state,
        "submitted": submitted,
        "conversation_id": r.conversation_id,
        "outcome": r.outcome,
        "error": r.error,
        "created_at": r.created_at,
        "updated_at": r.updated_at,
    })
}
