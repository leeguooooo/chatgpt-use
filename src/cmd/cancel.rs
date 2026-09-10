//! `cancel <request-id>` — stop the generation behind one `ask --request-id`
//! request, and say whether that is confirmed.
//!
//! With its owner alive, the owner does the stopping: it is signalled
//! (SIGTERM), presses stop in its own tab, which only ever shows its own
//! pinned conversation, and records the outcome in the receipt. With the owner
//! gone, this attaches to the recorded conversation and stops that one. Either
//! way nothing but that conversation can be touched, and `cancelled` is
//! reported only when the conversation record confirms it.

use crate::channel::{channel_error, Channel, ChannelOptions, ErrorKind};
use crate::cli::CancelArgs;
use crate::receipt;
use crate::structured;
use anyhow::Result;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub fn run(args: &CancelArgs) -> Result<()> {
    let mut envelope = cancel(args);
    envelope["request_id"] = args.request_id.as_str().into();
    let status = envelope["status"].as_str().unwrap_or("failed").to_string();
    crate::ledger::record("cancel", json!({"request_id": args.request_id, "status": status}));
    println!("{envelope}");
    let code = match status.as_str() {
        "cancelled" | "already_finished" => 0,
        other => structured::exit_code(other),
    };
    if code != 0 {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }
    Ok(())
}

fn cancel(args: &CancelArgs) -> Value {
    let id = &args.request_id;
    if !receipt::valid_id(id) {
        return json!({"status": "failed", "error": {"kind": "error", "message": format!("invalid request id {id:?}")}});
    }
    let path = receipt::path_for(id);
    let Some(r) = receipt::load(&path) else {
        return json!({"status": "failed", "error": {"kind": "unknown_request", "message": format!("no receipt for request {id:?}")}});
    };
    if matches!(r.state.as_str(), "completed" | "failed") {
        return settled(r.outcome.as_deref(), &r.submitted);
    }

    if receipt::pid_alive(r.pid) {
        return signal_owner(&path, r.pid);
    }

    let Some(convo) = r.conversation_id.clone() else {
        return json!({
            "status": "unknown",
            "error": {
                "kind": "no_conversation",
                "message": format!("request {id:?} has no recorded conversation and its owner is gone; \
                                    there is nothing to stop. Check the ChatGPT sidebar."),
                "submitted": if r.submitted == "yes" { "yes" } else { "unknown" },
            },
        });
    };

    let opts = ChannelOptions {
        profile: args.channel.profile.clone(),
        session: args.channel.session.clone(),
        project: String::new(),
        timeout_secs: args.channel.timeout,
        model: None,
        busy_fail: args.channel.busy == crate::cli::BusyPolicy::Fail,
        receipt: None,
    };
    let result = Channel::attach(&opts, &convo).and_then(|mut channel| {
        let result = channel.cancel_pinned();
        channel.close();
        result
    });
    let envelope = match result {
        Ok(_) => json!({"status": "already_finished", "outcome": "completed"}),
        Err(e) => match channel_error(&e).map(|c| c.kind) {
            Some(ErrorKind::Cancelled) => json!({"status": "cancelled", "message": format!("{e:#}")}),
            _ => structured::failure(&e),
        },
    };
    let mut for_receipt = envelope.clone();
    if envelope["status"] == "already_finished" {
        for_receipt = json!({"status": "completed"});
    } else if envelope["status"] == "cancelled" {
        for_receipt = json!({"status": "cancelled", "error": {"message": "cancelled", "submitted": "yes"}});
    }
    if matches!(for_receipt["status"].as_str(), Some("completed" | "cancelled")) {
        receipt::finish(&path, &for_receipt);
    }
    let mut envelope = envelope;
    envelope["conversation_id"] = convo.into();
    // Cancel never sends, so a failure here says nothing about submission; the
    // caller needs whether the ORIGINAL request reached ChatGPT.
    if envelope.get("error").is_some() {
        envelope["error"]["submitted"] = r.submitted.as_str().into();
    }
    envelope
}

/// Signal the live owner and wait for it to record how the cancel ended.
fn signal_owner(path: &std::path::Path, pid: u32) -> Value {
    let sent = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !sent {
        return json!({"status": "unknown", "error": {"kind": "signal_failed", "message": format!("could not signal owner pid {pid}")}});
    }
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if let Some(r) = receipt::load(path) {
            if matches!(r.state.as_str(), "completed" | "failed") {
                return settled(r.outcome.as_deref(), &r.submitted);
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    json!({
        "status": "cancel_requested",
        "message": format!("signalled owner pid {pid}; it has not recorded an outcome yet"),
    })
}

/// Report a request that has already ended, in cancel's terms.
fn settled(outcome: Option<&str>, submitted: &str) -> Value {
    let status = outcome_status(outcome);
    json!({"status": status, "outcome": outcome, "submitted": submitted})
}

/// A receipt outcome, as a cancel status. Only a recorded `cancelled` is a
/// confirmed cancel; a reply that exists means the request finished first.
fn outcome_status(outcome: Option<&str>) -> &'static str {
    match outcome {
        Some("cancelled") => "cancelled",
        Some("cancel_requested") => "cancel_requested",
        Some("completed" | "schema_violation" | "unparseable") => "already_finished",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_recorded_cancel_counts_as_cancelled() {
        assert_eq!(outcome_status(Some("cancelled")), "cancelled");
        assert_eq!(outcome_status(Some("cancel_requested")), "cancel_requested");
        assert_eq!(outcome_status(Some("completed")), "already_finished");
        assert_eq!(outcome_status(Some("schema_violation")), "already_finished");
        // A turn that died some other way was not cancelled by us.
        for other in [Some("incomplete"), Some("unavailable"), None] {
            assert_eq!(outcome_status(other), "unknown", "{other:?}");
        }
    }
}
