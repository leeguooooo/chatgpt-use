//! Subcommand entry points. Each is owned by a different agent and only ever
//! edits its own file; this module just declares them.

pub mod ask; // Mode 1 — sidekick / structured delegation
pub mod handoff; // executor handoff — feed a packet to Codex / Claude Code
pub mod init; // one-time setup — generate ~/.chatgpt-use/auth.json
pub mod mcp; // MCP channel — expose project tools to a regular GPT-5.5
pub mod refresh; // refresh the connector in ChatGPT settings (re-run tools/list)
pub mod run; // Mode 2 — brain
pub mod serve; // Mode 3 — drop-in model
pub mod work; // closed loop — ChatGPT does the work via its MCP connector
