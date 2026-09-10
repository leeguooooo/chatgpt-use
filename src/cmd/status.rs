//! `status <request-id>` — report what happened to an `ask --request-id`
//! request, from its receipt alone. It never touches the browser, so it
//! answers even while another run holds the ChatGPT window.

use crate::cli::StatusArgs;
use crate::receipt;
use anyhow::{bail, Result};

pub fn run(args: &StatusArgs) -> Result<()> {
    if !receipt::valid_id(&args.request_id) {
        bail!("invalid request id {:?}", args.request_id);
    }
    let Some(r) = receipt::load(&receipt::path_for(&args.request_id)) else {
        bail!("no receipt for request {:?}", args.request_id);
    };
    let state = receipt::live_state(&r, receipt::pid_alive);
    // The receipt's "no" only meant "not recorded yet" while the owner ran; an
    // owner that died before recording anything may still have pressed Enter.
    let submitted = if state == "submission_unknown" { "unknown" } else { r.submitted.as_str() };
    println!(
        "{}",
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
    );
    Ok(())
}
