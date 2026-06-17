//! Local tool executor — the "hands" in Mode 2 (and the tools Claude Code's
//! calls map onto in Mode 3). Reads/writes files and runs commands on THIS
//! machine, then hands observations back into the conversation. This is also
//! why ChatGPT gets "file access" without any tunnel: the bytes are read here.
//!
//! Owned by the CORE agent.

use crate::protocol::{ToolCall, ToolResult, ToolSpec};
use std::path::Path;

/// The built-in tool catalog advertised to the model.
///
/// CORE agent, implement at least:
///   read_file {path}                 -> file contents
///   write_file {path, content}       -> write/overwrite (side-effecting)
///   list_dir {path}                  -> directory listing
///   grep {pattern, path?}            -> matching lines (use the `regex` crate)
///   bash {command}                   -> run a shell command (side-effecting)
pub fn builtin_specs() -> Vec<ToolSpec> {
    todo!("CORE: declare the built-in tools with JSON-Schema inputs")
}

/// Execute one tool call against `cwd`.
///
/// `auto_approve == false` means side-effecting tools (write_file, bash) must
/// prompt for interactive confirmation before running; read-only tools never
/// prompt. Confine paths to `cwd` and surface errors as `ok: false` results
/// rather than panicking.
pub fn execute(_call: &ToolCall, _cwd: &Path, _auto_approve: bool) -> ToolResult {
    todo!("CORE: dispatch on call.name, run it, capture output into ToolResult")
}
