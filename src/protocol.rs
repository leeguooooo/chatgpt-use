//! The text tool-call protocol — the heart of Modes 2 and 3.
//!
//! Web ChatGPT has no native function-calling, so we define a text protocol in
//! the system prompt and parse tool calls back out of the assistant's reply.
//! This module owns the shared vocabulary (ToolSpec / ToolCall / ToolResult),
//! the system-prompt text, the reply parser, and the result renderer.
//!
//! Owned by the CORE agent. See README "The honest caveats" for why robustness
//! (strict format + repair re-ask) matters here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool the model is allowed to call, advertised in the system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input object.
    pub input_schema: Value,
}

/// A single tool invocation parsed out of the model's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Correlation id (so results can be matched back, incl. Anthropic tool_use_id).
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// The outcome of executing a ToolCall (produced by the `tools` module).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub ok: bool,
    pub content: String,
}

/// What the model's reply decoded to: either it's done (free text) or it wants
/// to call one or more tools.
#[derive(Debug, Clone)]
pub enum Reply {
    /// The model produced a final textual answer (no tool calls).
    Text(String),
    /// The model requested tool calls.
    Tools(Vec<ToolCall>),
}

// ---- Deserialization helpers ------------------------------------------------

/// The exact JSON shape expected inside a tool-call fenced block.
#[derive(Debug, Deserialize)]
struct ToolCallEnvelope {
    tool_calls: Vec<ToolCallRaw>,
}

#[derive(Debug, Deserialize)]
struct ToolCallRaw {
    /// Models drop the id when they emit a single call; correlate positionally then.
    #[serde(default)]
    id: Option<String>,
    name: String,
    /// The model picks its own word for the argument bag — observed live as both
    /// `input` and `arguments`. Accept the common variants, and tolerate it being
    /// omitted entirely for a no-argument tool.
    #[serde(default, alias = "arguments", alias = "parameters", alias = "args")]
    input: Value,
}

// ---- Public API ------------------------------------------------------------

/// Build the system prompt that teaches ChatGPT the tool protocol.
///
/// The format is a single fenced ```json block containing:
///   {"tool_calls":[{"id":"...","name":"...","input":{...}}]}
/// When finished, ChatGPT must respond with plain prose and NO json block.
pub fn system_prompt(tools: &[ToolSpec], task: &str) -> String {
    // Render the tool catalog as a JSON array.
    let catalog = serde_json::to_string_pretty(tools).unwrap_or_else(|_| "[]".to_string());

    format!(
        r#"You are the reasoning core of an automated coding system. The text below is a
MACHINE DELEGATION sent by an automated controller program — it is NOT a message
from a human, and this is NOT a casual chat.

Read this carefully, it removes a common confusion:
You are NOT executing anything yourself, and the question of whether *you* "have
tools" does not apply. You simply EMIT a JSON request describing an action you
want taken. A separate controller program running on the user's machine parses
your request, actually performs it, and sends you the real result back as a
`tool_result`. Therefore: NEVER reply "I can't run those tools" or "please paste
the file" — that is a category error here. To read a file you REQUEST it via the
protocol and the controller returns its contents. This loop genuinely works.

Task to accomplish:

{task}

## Protocol

To request one or more actions, output ONLY this fenced JSON block, nothing else:

```json
{{"tool_calls":[{{"id":"call_1","name":"read_file","input":{{"path":"src/main.rs"}}}}]}}
```

When the task is fully complete, instead output a plain-text final answer with NO
json block. Every turn is EITHER one tool_calls block OR a plain-text final
answer — never both, never neither.

## Worked example (shows the controller really runs your requests)

[delegation] Read Cargo.toml and report the version.
you →
```json
{{"tool_calls":[{{"id":"call_1","name":"read_file","input":{{"path":"Cargo.toml"}}}}]}}
```
[controller] tool_result call_1 (ok): [package] name = "demo" version = "0.3.7"
you →
0.3.7

## Rules

- The block starts with ```json on its own line and ends with ``` on its own line.
- Exactly ONE block per turn; no prose before or after it.
- Unique `id` per call within a turn (call_1, call_2, …).
- If a previous reply was rejected as invalid, re-emit ONLY a valid JSON block.

## Available tools

{catalog}

## Begin

Emit your first tool_calls block now. Do not greet, explain, or ask for files."#,
        task = task,
        catalog = catalog
    )
}

/// Parse one assistant message into a Reply.
///
/// Looks for a fenced ```json block containing a `tool_calls` array.
/// Tolerates the block being wrapped in prose (whitespace, text before/after).
/// On a malformed would-be block (present but unparseable), returns `Reply::Text`
/// so the caller can re-ask rather than crash — in keeping with the README's
/// robustness goal.
pub fn parse_reply(assistant_text: &str) -> Reply {
    // IMPORTANT: this runs on RENDERED text — the innerText we scrape out of the
    // DOM — not on markdown source. By the time a reply reaches us, a ```json
    // fence has already been rendered away into a code block whose innerText is
    // a language-label line ("JSON") followed by the code. A fence-matching
    // regex therefore NEVER matched a real reply, so every tool call was
    // misread as a plain-text final answer and the agent loop exited on turn one.
    //
    // So: scan the whole reply for a balanced JSON object that actually carries
    // `tool_calls`, wherever it sits — bare, label-prefixed, or (when a literal
    // fence does survive) inside one.
    match find_envelope(assistant_text) {
        Some(env) => {
            let calls = env
                .tool_calls
                .into_iter()
                .enumerate()
                .map(|(i, r)| ToolCall {
                    id: r.id.unwrap_or_else(|| format!("call_{i}")),
                    name: r.name,
                    input: r.input,
                })
                .collect();
            Reply::Tools(calls)
        }
        // No tool-call envelope anywhere — this is the plain-text final answer.
        None => Reply::Text(assistant_text.to_string()),
    }
}

/// Find the first balanced `{...}` in `text` that deserializes into a non-empty
/// tool-call envelope.
fn find_envelope(text: &str) -> Option<ToolCallEnvelope> {
    for (start, _) in text.char_indices().filter(|(_, c)| *c == '{') {
        let Some(end) = balanced_object_end(text, start) else { continue };
        let candidate = &text[start..end];
        // Cheap prefilter: skip objects that can't be an envelope.
        if !candidate.contains("tool_calls") {
            continue;
        }
        if let Ok(env) = serde_json::from_str::<ToolCallEnvelope>(candidate) {
            if !env.tool_calls.is_empty() {
                return Some(env);
            }
        }
    }
    None
}

/// Byte index just past the `}` that closes the object opening at `start`.
///
/// Brace-counting alone is not enough: a brace inside a string literal (a tool
/// argument containing `{`, a regex, a code snippet) would unbalance the count,
/// so string literals and their escapes are skipped.
fn balanced_object_end(text: &str, start: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &c) in b.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Render tool results into the next user turn fed back to ChatGPT.
///
/// Each result is presented under its correlation id so the model can match
/// observations back to the calls it made.
pub fn render_results(results: &[ToolResult]) -> String {
    if results.is_empty() {
        return "No tool results.".to_string();
    }

    let mut out = String::from("Tool results:\n\n");
    for res in results {
        let status = if res.ok { "ok" } else { "error" };
        out.push_str(&format!("### Result for `{}` ({})\n\n", res.id, status));
        out.push_str(&res.content);
        out.push_str("\n\n");
    }

    out.push_str(
        "Continue: call more tools as needed, or respond with plain text when done.",
    );
    out
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// The regression that mattered: we parse RENDERED innerText, where a
    /// ```json fence has become a "JSON" label line + the code. The old
    /// fence-matching parser returned Text here, so the agent loop exited on
    /// turn one instead of running the tool.
    #[test]
    fn parses_a_rendered_code_block_with_a_language_label() {
        let rendered = "JSON\n{\"tool_calls\":[{\"id\":\"call_0\",\"name\":\"list_dir\",\"input\":{\"path\":\".\"}}]}";
        match parse_reply(rendered) {
            Reply::Tools(c) => {
                assert_eq!(c.len(), 1);
                assert_eq!(c[0].name, "list_dir");
                assert_eq!(c[0].id, "call_0");
            }
            Reply::Text(t) => panic!("expected a tool call, got text: {t}"),
        }
    }

    #[test]
    fn parses_a_bare_single_line_envelope() {
        // What the priming nudge actually asks the model to emit.
        let bare = r#"{"tool_calls":[{"id":"call_0","name":"list_dir","input":{"path":"."}}]}"#;
        assert!(matches!(parse_reply(bare), Reply::Tools(_)));
    }

    #[test]
    fn parses_an_envelope_surrounded_by_prose() {
        let msg = "Sure, let me look.\n{\"tool_calls\":[{\"id\":\"a\",\"name\":\"list_dir\",\"input\":{}}]}\nStanding by.";
        assert!(matches!(parse_reply(msg), Reply::Tools(_)));
    }

    /// Observed live: the model wrote `arguments` instead of `input`, and the
    /// strict struct rejected the whole envelope.
    #[test]
    fn accepts_arguments_as_an_alias_for_input() {
        let msg = r#"{"tool_calls":[{"id":"call_1","name":"list_dir","arguments":{}}]}"#;
        match parse_reply(msg) {
            Reply::Tools(c) => assert_eq!(c[0].name, "list_dir"),
            Reply::Text(t) => panic!("expected a tool call, got text: {t}"),
        }
    }

    #[test]
    fn synthesizes_an_id_when_the_model_omits_it() {
        let msg = r#"{"tool_calls":[{"name":"list_dir","input":{}}]}"#;
        match parse_reply(msg) {
            Reply::Tools(c) => assert_eq!(c[0].id, "call_0"),
            Reply::Text(t) => panic!("expected a tool call, got text: {t}"),
        }
    }

    /// A brace inside a string argument must not unbalance the scanner.
    #[test]
    fn scans_past_braces_inside_string_arguments() {
        let msg = r#"{"tool_calls":[{"id":"g","name":"grep","input":{"pattern":"fn \\w+\\{"}}]}"#;
        match parse_reply(msg) {
            Reply::Tools(c) => assert_eq!(c[0].name, "grep"),
            Reply::Text(t) => panic!("expected a tool call, got text: {t}"),
        }
    }

    /// A prose answer that merely mentions the word must stay a final answer.
    #[test]
    fn plain_prose_is_still_a_final_answer() {
        let msg = "I could not find any tool_calls to make; the version is 0.0.1.";
        assert!(matches!(parse_reply(msg), Reply::Text(_)));
    }

    use super::*;
    use serde_json::json;

    fn make_tool_result(id: &str, ok: bool, content: &str) -> ToolResult {
        ToolResult {
            id: id.to_string(),
            ok,
            content: content.to_string(),
        }
    }

    // --- parse_reply ---

    #[test]
    fn parse_clean_tool_call_block() {
        let text = r#"```json
{"tool_calls":[{"id":"call_1","name":"read_file","input":{"path":"src/main.rs"}}]}
```"#;
        match parse_reply(text) {
            Reply::Tools(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_1");
                assert_eq!(calls[0].name, "read_file");
                assert_eq!(calls[0].input["path"], "src/main.rs");
            }
            Reply::Text(t) => panic!("expected Tools, got Text: {t:?}"),
        }
    }

    #[test]
    fn parse_tool_call_block_wrapped_in_prose() {
        // Model sometimes adds a sentence before or after the block.
        let text = r#"Let me read the file first.

```json
{"tool_calls":[{"id":"call_1","name":"read_file","input":{"path":"Cargo.toml"}}]}
```

I'll review it next."#;
        match parse_reply(text) {
            Reply::Tools(calls) => {
                assert_eq!(calls[0].name, "read_file");
            }
            Reply::Text(t) => panic!("expected Tools, got Text: {t:?}"),
        }
    }

    #[test]
    fn parse_malformed_block_falls_back_to_text() {
        // A block that looks like tool_calls but has broken JSON.
        let text = r#"```json
{"tool_calls":[BROKEN}
```"#;
        match parse_reply(text) {
            Reply::Text(_) => {} // correct: graceful fallback
            Reply::Tools(_) => panic!("should have fallen back to Text on malformed JSON"),
        }
    }

    #[test]
    fn parse_no_block_is_plain_text() {
        let text = "I have finished the task. The code compiles and tests pass.";
        match parse_reply(text) {
            Reply::Text(t) => assert_eq!(t, text),
            Reply::Tools(_) => panic!("expected Text for prose-only reply"),
        }
    }

    #[test]
    fn parse_multiple_tool_calls_in_one_block() {
        let text = r#"```json
{"tool_calls":[
  {"id":"call_1","name":"read_file","input":{"path":"a.rs"}},
  {"id":"call_2","name":"list_dir","input":{"path":"src"}}
]}
```"#;
        match parse_reply(text) {
            Reply::Tools(calls) => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[1].name, "list_dir");
            }
            Reply::Text(t) => panic!("expected Tools, got Text: {t:?}"),
        }
    }

    // --- render_results ---

    #[test]
    fn render_results_includes_id_and_content() {
        let results = vec![
            make_tool_result("call_1", true, "fn main() {}"),
            make_tool_result("call_2", false, "permission denied"),
        ];
        let rendered = render_results(&results);
        assert!(rendered.contains("call_1"), "should include first id");
        assert!(rendered.contains("call_2"), "should include second id");
        assert!(rendered.contains("fn main()"), "should include content");
        assert!(rendered.contains("permission denied"), "should include error");
        assert!(rendered.contains("ok"), "should label successful result");
        assert!(rendered.contains("error"), "should label failed result");
    }

    #[test]
    fn render_empty_results() {
        let rendered = render_results(&[]);
        assert!(!rendered.is_empty());
    }

    // --- system_prompt ---

    #[test]
    fn system_prompt_contains_task_and_protocol_hint() {
        let tools: Vec<ToolSpec> = vec![];
        let prompt = system_prompt(&tools, "Add a --json flag to the status command.");
        assert!(prompt.contains("--json flag"), "should embed the task");
        assert!(prompt.contains("tool_calls"), "should describe the tool-call format");
        assert!(
            prompt.contains("```json"),
            "should show the fenced block example"
        );
    }

    #[test]
    fn system_prompt_includes_tool_names() {
        let tools = vec![ToolSpec {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }];
        let prompt = system_prompt(&tools, "some task");
        assert!(prompt.contains("read_file"), "should embed tool name");
    }
}
