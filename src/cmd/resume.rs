//! `resume <request-id>` — pick up an `ask --request-id` request whose caller
//! lost it (a timeout, a crash, a kill) by waiting for its reply on the
//! server. It never sends anything: the whole point of a receipt is that a
//! lost reply is looked up, not asked for twice.
//!
//! Prints one JSON envelope, like `ask --output-schema`: validated against
//! `--output-schema` if given, else `{"status":"completed","result":{"text":…}}`
//! (the CLI also repeats the text at the top level, as `text`, for older callers).

use crate::channel::{Channel, ChannelOptions};
use crate::cli::ResumeArgs;
use crate::receipt::{self, Receipt};
use crate::structured;
use anyhow::Result;
use serde_json::{json, Value};

pub fn run(args: &ResumeArgs) -> Result<()> {
    let mut envelope = resume(args, None, None);
    // Older callers read the reply text at the top level; keep it there too.
    if let Some(text) = envelope["result"]["text"].as_str().map(str::to_string) {
        envelope["text"] = text.into();
    }
    envelope["request_id"] = args.request_id.as_str().into();
    let status = envelope["status"].as_str().unwrap_or("failed").to_string();
    crate::ledger::record("resume", json!({"request_id": args.request_id, "status": status}));
    println!("{envelope}");
    let code = structured::exit_code(&status);
    if code != 0 {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }
    Ok(())
}

/// The resume core: one envelope, never prints or exits. `owner` (kind and
/// token) is recorded in the receipt while this process runs the request
/// (see `Receipt::owner`). `inline_schema` takes the place of
/// `--output-schema` for callers holding the schema as a value.
pub(crate) fn resume(
    args: &ResumeArgs,
    owner: Option<(&str, &str)>,
    inline_schema: Option<Value>,
) -> Value {
    let id = &args.request_id;
    if !receipt::valid_id(id) {
        return refusal("failed", "error", &format!("invalid request id {id:?}"), "no");
    }
    let path = receipt::path_for(id);
    let Some(r) = receipt::load(&path) else {
        return refusal("failed", "unknown_request", &format!("no receipt for request {id:?}"), "no");
    };
    let convo = match plan(&r, receipt::pid_alive) {
        Ok(convo) => convo,
        Err(envelope) => return envelope,
    };
    let loaded = match (inline_schema, &args.output_schema) {
        (Some(v), _) => Some(structured::Schema::from_value(v)),
        (None, Some(p)) => Some(structured::Schema::load(p)),
        (None, None) => None,
    };
    let schema = match loaded {
        None => None,
        Some(Ok(s)) => Some(s),
        Some(Err(why)) => return structured::schema_error(&why),
    };

    // Take ownership and mark it running while we wait, so `status` and
    // `cancel` treat it as live and reach THIS process, not a settled record.
    let prior = r.clone();
    receipt::update(&path, |r| {
        r.state = "submitted".into();
        r.pid = std::process::id();
        r.owner = owner.map(|(kind, _)| kind.to_string());
        r.owner_token = owner.map(|(_, token)| token.to_string());
    });
    let opts = ChannelOptions {
        profile: args.channel.profile.clone(),
        session: args.channel.session.clone(),
        project: String::new(),
        timeout_secs: args.channel.timeout,
        model: None,
        busy_fail: args.channel.busy == crate::cli::BusyPolicy::Fail,
        receipt: None,
        ignore_cooldown: false,
    };
    let reply = Channel::attach(&opts, &convo).and_then(|mut channel| {
        let reply = channel.await_record();
        channel.close();
        reply
    });
    let mut envelope = match (reply, &schema) {
        (reply, Some(schema)) => structured::evaluate(reply, schema),
        (Ok(text), None) => json!({"status": "completed", "result": {"text": text}}),
        (Err(e), None) => structured::failure(&e),
    };
    envelope["conversation_id"] = convo.as_str().into();
    // Resume never sends, so a failure here says nothing about submission; what
    // the caller needs is whether the ORIGINAL request reached ChatGPT.
    if envelope.get("error").is_some() {
        envelope["error"]["submitted"] = r.submitted.as_str().into();
    }
    match after_resume(&prior, &envelope) {
        Settle::Finish => receipt::finish(&path, &envelope),
        Settle::Restore => receipt::update(&path, |r| {
            r.state = prior.state.clone();
            r.submitted = prior.submitted.clone();
            r.outcome = prior.outcome.clone();
            r.error = prior.error.clone();
        }),
    }
    envelope
}

#[derive(Debug, PartialEq, Eq)]
enum Settle {
    /// Record this resume's outcome.
    Finish,
    /// Put the receipt back as it was: a re-read that failed (throttled,
    /// cancelled, timed out) does not undo a request that had completed.
    Restore,
}

fn after_resume(prior: &Receipt, envelope: &Value) -> Settle {
    let replied = matches!(
        envelope["status"].as_str(),
        Some("completed" | "schema_violation" | "unparseable")
    );
    if !replied && prior.state == "completed" {
        Settle::Restore
    } else {
        Settle::Finish
    }
}

/// What a receipt allows: the conversation to attach to, or the envelope that
/// explains why there is nothing to attach to. Never "send it again".
fn plan(r: &Receipt, alive: impl Fn(u32) -> bool) -> Result<String, Value> {
    let id = &r.request_id;
    let state = receipt::live_state(r, alive);
    if state == "running" {
        return Err(refusal(
            "busy",
            "busy",
            &format!("request {id:?} is still running (pid {}); not attaching to it", r.pid),
            &r.submitted,
        ));
    }
    if r.state == "failed" && r.submitted == "no" {
        return Err(refusal(
            "failed",
            "not_submitted",
            &format!("request {id:?} was never sent, so there is nothing to resume; send it again with ask"),
            "no",
        ));
    }
    match &r.conversation_id {
        Some(convo) => Ok(convo.clone()),
        // Sent (or maybe sent) before ChatGPT assigned a conversation id: there
        // is nothing to attach to, and resending could post it twice.
        None => Err(refusal(
            "submission_unknown",
            "no_conversation",
            &format!(
                "request {id:?} has no recorded conversation (state {state}); there is nothing \
                 to attach to, and it will not be resent. Check the ChatGPT sidebar."
            ),
            if r.submitted == "yes" { "yes" } else { "unknown" },
        )),
    }
}

fn refusal(status: &str, kind: &str, message: &str, submitted: &str) -> Value {
    json!({"status": status, "error": {"kind": kind, "message": message, "submitted": submitted}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(state: &str, submitted: &str, convo: Option<&str>) -> Receipt {
        let mut r = Receipt::accepted("r1");
        r.state = state.into();
        r.submitted = submitted.into();
        r.conversation_id = convo.map(str::to_string);
        r
    }

    #[test]
    fn a_failed_reread_never_undoes_a_completed_request() {
        let done = receipt("completed", "yes", Some("c-1"));
        assert_eq!(after_resume(&done, &json!({"status": "unavailable"})), Settle::Restore);
        assert_eq!(after_resume(&done, &json!({"status": "cancelled"})), Settle::Restore);
        assert_eq!(after_resume(&done, &json!({"status": "completed"})), Settle::Finish);
        // An unfinished one takes whatever this attempt found out.
        let lost = receipt("submitted", "yes", Some("c-1"));
        assert_eq!(after_resume(&lost, &json!({"status": "incomplete"})), Settle::Finish);
        assert_eq!(after_resume(&lost, &json!({"status": "completed"})), Settle::Finish);
    }

    #[test]
    fn attaches_to_a_detached_request_with_a_conversation() {
        let r = receipt("submitted", "yes", Some("c-1"));
        assert_eq!(plan(&r, |_| false), Ok("c-1".to_string()));
        // A completed one can be re-read too.
        let r = receipt("completed", "yes", Some("c-1"));
        assert_eq!(plan(&r, |_| false), Ok("c-1".to_string()));
    }

    #[test]
    fn never_attaches_under_a_live_owner() {
        let r = receipt("submitted", "yes", Some("c-1"));
        assert_eq!(plan(&r, |_| true).unwrap_err()["status"], "busy");
    }

    #[test]
    fn a_request_without_a_conversation_is_submission_unknown_not_resent() {
        for (state, submitted) in [("accepted", "no"), ("submitted", "yes"), ("failed", "unknown")] {
            let env = plan(&receipt(state, submitted, None), |_| false).unwrap_err();
            assert_eq!(env["status"], "submission_unknown", "{state}/{submitted}");
            assert_ne!(env["error"]["submitted"], "no", "{state}/{submitted}");
        }
    }

    #[test]
    fn a_request_proven_unsent_is_left_to_ask() {
        let env = plan(&receipt("failed", "no", None), |_| false).unwrap_err();
        assert_eq!(env["error"]["kind"], "not_submitted");
    }
}
