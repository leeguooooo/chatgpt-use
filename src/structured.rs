//! `ask --output-schema FILE`: ask for one JSON value, validate it locally
//! against the caller's JSON Schema, and report the outcome as ONE envelope on
//! stdout with an explicit status.
//!
//! The rule this module exists to enforce: a turn that failed, stalled or
//! answered in the wrong shape must never read as an answer. `completed` means
//! `result` exists and validated; every other status says what went wrong and,
//! for channel failures, whether the prompt reached ChatGPT.
//!
//! There are no repair turns. A reply that fails validation is reported as
//! `schema_violation` with the raw text: asking the model to fix its JSON would
//! be another paid turn the caller did not ask for. A caller that wants a retry
//! can make one.

use crate::channel::channel_error;
use serde_json::{json, Value};

/// A caller's schema, compiled once.
pub struct Schema {
    source: Value,
    validator: jsonschema::Validator,
}

impl Schema {
    pub fn load(path: &str) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read schema {path}: {e}"))?;
        let source: Value = serde_json::from_str(&text)
            .map_err(|e| format!("schema {path} is not valid JSON: {e}"))?;
        Self::from_value(source)
    }

    pub fn from_value(source: Value) -> Result<Self, String> {
        let validator = jsonschema::validator_for(&source).map_err(|e| {
            // Built without the HTTP client, so a remote $ref cannot resolve.
            // That is by design; say so rather than leave a resolver error.
            let msg = e.to_string();
            if msg.contains("$ref") || msg.contains("etriev") || msg.contains("esolv") {
                format!("{msg} (schemas must be self-contained: external $ref is not fetched)")
            } else {
                msg
            }
        })?;
        Ok(Schema { source, validator })
    }
}

/// The request plus the instruction to answer in the schema's shape.
pub fn build_message(request: &str, schema: &Schema) -> String {
    format!(
        "{request}\n\n---\nAnswer with exactly one JSON value that validates against the JSON \
         Schema below. Output only that JSON, with no explanation before or after it.\n\n\
         ```json\n{}\n```",
        serde_json::to_string_pretty(&schema.source).unwrap_or_default()
    )
}

/// Judge one turn's outcome.
pub fn evaluate(reply: anyhow::Result<String>, schema: &Schema) -> Value {
    let raw = match reply {
        Ok(text) => text,
        Err(e) => return failure(&e),
    };
    let value = match extract(&raw) {
        Ok(v) => v,
        Err(why) => {
            return json!({
                "status": "unparseable",
                "error": {"kind": "unparseable", "message": why},
                "raw": raw,
            })
        }
    };
    let errors: Vec<Value> = schema
        .validator
        .iter_errors(&value)
        .map(|e| json!({"path": e.instance_path().to_string(), "message": e.to_string()}))
        .collect();
    if errors.is_empty() {
        json!({"status": "completed", "result": value})
    } else {
        json!({"status": "schema_violation", "errors": errors, "result": value, "raw": raw})
    }
}

/// The envelope for a failure that produced no reply to judge.
pub fn failure(e: &anyhow::Error) -> Value {
    let message = format!("{e:#}");
    match channel_error(e) {
        Some(ce) => json!({
            "status": ce.kind.status(),
            "error": {"kind": ce.kind.as_str(), "message": message, "submitted": ce.submitted.as_str()},
        }),
        // Untyped errors come from before any turn (a context file that will
        // not read, chrome-use missing): `send` types everything it returns.
        None => json!({
            "status": "failed",
            "error": {"kind": "error", "message": message, "submitted": "no"},
        }),
    }
}

/// The envelope for a schema that could not be loaded or compiled. The
/// caller's input is at fault, and nothing was sent.
pub fn schema_error(why: &str) -> Value {
    json!({"status": "schema_error", "error": {"kind": "schema_error", "message": why, "submitted": "no"}})
}

/// Exit status per envelope status, so a shell caller can branch without
/// parsing. 0 is `completed` only; 2 is left to clap's usage errors.
pub fn exit_code(status: &str) -> i32 {
    match status {
        "completed" => 0,
        "schema_violation" => 3,
        "unparseable" => 4,
        "incomplete" => 5,
        "unavailable" => 6,
        "busy" => 7,
        "schema_error" => 8,
        "duplicate" => 9,
        "submission_unknown" => 10,
        _ => 1,
    }
}

/// The one JSON value in a reply: the whole reply if it parses, else a
/// fenced ```json block, else the first balanced object in the prose.
fn extract(reply: &str) -> Result<Value, String> {
    let text = reply.trim();
    if text.is_empty() {
        return Err("the reply was empty".into());
    }
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Ok(v);
    }
    let candidate = crate::delegation::extract_json_object(text)
        .ok_or_else(|| "no JSON value found in the reply".to_string())?;
    serde_json::from_str(&candidate).map_err(|e| format!("the JSON in the reply does not parse: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ChannelError, ErrorKind};

    fn review_schema() -> Schema {
        Schema::from_value(json!({
            "type": "object",
            "required": ["status", "findings"],
            "properties": {
                "status": {"enum": ["completed"]},
                "findings": {"type": "array", "items": {
                    "type": "object",
                    "required": ["severity", "file", "line", "summary"],
                    "properties": {
                        "severity": {"type": "string", "pattern": "^P[0-3]$"},
                        "file": {"type": "string"},
                        "line": {"type": "integer", "minimum": 1},
                        "summary": {"type": "string"}
                    }
                }}
            }
        }))
        .unwrap()
    }

    fn ok(reply: &str) -> Value {
        evaluate(Ok(reply.to_string()), &review_schema())
    }

    #[test]
    fn a_valid_empty_review_is_completed_with_its_result() {
        let env = ok(r#"{"status":"completed","findings":[]}"#);
        assert_eq!(env["status"], "completed");
        assert_eq!(env["result"]["findings"], json!([]));
    }

    #[test]
    fn prose_wrapped_json_is_found_fenced_or_not() {
        let fenced = "Here is the review:\n```json\n{\"status\":\"completed\",\"findings\":[]}\n```\nLet me know.";
        assert_eq!(ok(fenced)["status"], "completed");
        let bare = "Review done. {\"status\":\"completed\",\"findings\":[{\"severity\":\"P2\",\
                    \"file\":\"a.rs\",\"line\":3,\"summary\":\"x {braces} in a string\"}]} Thanks!";
        let env = ok(bare);
        assert_eq!(env["status"], "completed");
        assert_eq!(env["result"]["findings"][0]["line"], 3);
    }

    #[test]
    fn malformed_or_truncated_json_is_unparseable_not_completed() {
        for reply in [
            r#"{"status":"completed","findings":[}"#,
            // A reply cut off mid-generation.
            r#"{"status":"completed","findings":[{"severity":"P1","file":"a.rs""#,
            "I could not review this diff.",
            "   ",
        ] {
            let env = ok(reply);
            assert_eq!(env["status"], "unparseable", "{reply:?}");
            assert!(env.get("result").is_none(), "{reply:?}");
            assert_eq!(env["raw"], reply);
        }
    }

    #[test]
    fn missing_and_mistyped_fields_are_rejected_with_paths() {
        let env = ok(r#"{"status":"completed"}"#);
        assert_eq!(env["status"], "schema_violation");
        assert!(env["errors"][0]["message"].as_str().unwrap().contains("findings"));

        let env = ok(r#"{"status":"completed","findings":[{"severity":"P9","file":"a","line":"42","summary":"s"}]}"#);
        assert_eq!(env["status"], "schema_violation");
        let paths: Vec<&str> =
            env["errors"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap()).collect();
        assert!(paths.contains(&"/findings/0/severity"), "{paths:?}");
        assert!(paths.contains(&"/findings/0/line"), "{paths:?}");
    }

    #[test]
    fn channel_failures_keep_their_kind_and_submission_state() {
        let incomplete: anyhow::Error = ChannelError::new(ErrorKind::Incomplete, "timed out").into();
        let env = evaluate(Err(incomplete), &review_schema());
        assert_eq!(env["status"], "incomplete");
        assert_eq!(env["error"]["kind"], "incomplete");
        assert!(env.get("result").is_none());

        let limited: anyhow::Error = ChannelError::new(ErrorKind::RateLimited, "429").into();
        let env = evaluate(Err(limited.context("connecting")), &review_schema());
        assert_eq!(env["status"], "unavailable");
        assert_eq!(env["error"]["kind"], "rate_limited");
        assert_eq!(env["error"]["submitted"], "no");

        let env = failure(&anyhow::anyhow!("failed to read context file: x"));
        assert_eq!(env["status"], "failed");
    }

    #[test]
    fn a_remote_ref_is_a_schema_error_that_says_why() {
        let err = Schema::from_value(json!({"$ref": "https://example.com/review.json"})).err();
        let why = err.expect("a remote $ref must not compile without the HTTP client");
        assert!(why.contains("self-contained"), "{why}");
        assert_eq!(schema_error(&why)["status"], "schema_error");
    }

    #[test]
    fn only_completed_exits_zero_and_each_status_has_its_own_code() {
        let statuses =
            ["completed", "schema_violation", "unparseable", "incomplete", "unavailable", "busy", "schema_error", "duplicate", "submission_unknown", "failed"];
        let codes: Vec<i32> = statuses.iter().map(|s| exit_code(s)).collect();
        assert_eq!(codes.iter().filter(|&&c| c == 0).count(), 1);
        let mut unique = codes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "{codes:?}");
    }
}
