//! MCP channel — a local MCP server that exposes this project's tools
//! (read_file / write_file / list_dir / grep / bash, from `crate::tools`) to a
//! *regular* GPT-5.5 conversation. Regular 5.5 can call native MCP tools, so this
//! is the no-role-play path (the browser channel can't do tools; this can).
//!
//! Because ChatGPT runs in the cloud and can't reach localhost, the operator
//! exposes this server via a public tunnel (Cloudflare Tunnel / ngrok / Tailscale)
//! and registers it in ChatGPT > Settings > Apps as a custom MCP connector. A
//! shared `--token` gates access (compare in constant time; only the value the
//! operator copies into the connector should work). NOTE: Pro cannot use MCP
//! connectors — this channel is for regular GPT-5.5 only.
//!
//! Implement JSON-RPC 2.0 MCP over HTTP using `tiny_http` (and `serde_json`):
//!   - `initialize` → server capabilities
//!   - `tools/list` → map `crate::tools::builtin_specs()` to MCP tool descriptors
//!   - `tools/call` → dispatch to `crate::tools::execute(&ToolCall, cwd, true)`
//!     and return the result as MCP tool content
//! Reuse `crate::tools` and `crate::protocol::{ToolCall, ToolSpec}` — do not
//! reimplement the executor.
//!
//! Owned by the MCP agent.

use crate::cli::McpArgs;
use anyhow::Result;

pub fn run(_args: &McpArgs) -> Result<()> {
    todo!("MCP: tiny_http JSON-RPC server exposing crate::tools to a regular GPT-5.5")
}
