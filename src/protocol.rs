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

/// Build the system prompt that teaches ChatGPT the tool protocol.
///
/// CORE agent: define a strict, unambiguous format (recommended: a single
/// ```json fenced block ```with `{"tool_calls":[{"id","name","input"}]}`), and
/// instruct the model to emit ONLY that block when calling tools, or plain
/// prose (no block) when finished.
pub fn system_prompt(_tools: &[ToolSpec], _task: &str) -> String {
    todo!("CORE: render protocol instructions + tool catalog")
}

/// Parse one assistant message into a Reply. Must tolerate the model wrapping
/// the block in prose, stray whitespace, and minor format drift; on an
/// unparseable would-be tool call, prefer returning Text so the loop can
/// re-ask rather than crash.
pub fn parse_reply(_assistant_text: &str) -> Reply {
    todo!("CORE: extract the fenced tool-call JSON; fall back to Text")
}

/// Render tool results into the next user turn fed back to ChatGPT.
pub fn render_results(_results: &[ToolResult]) -> String {
    todo!("CORE: format observations clearly, keyed by tool-call id")
}
