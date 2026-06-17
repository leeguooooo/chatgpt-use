//! CLI argument definitions (clap). Owned by the orchestrator; the cmd modules
//! consume these structs but do not modify this file.

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "chatgpt-use",
    version,
    about = "Drive your ChatGPT web subscription as a coding-agent backend (no API key)."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Mode 1 · sidekick — one-shot question with optional file context, no tools.
    Ask(AskArgs),
    /// Mode 2 · brain — ChatGPT drives local tools in an agent loop until done.
    Run(RunArgs),
    /// Mode 3 · drop-in — Anthropic-compatible /v1/messages shim for Claude Code.
    Serve(ServeArgs),
    /// MCP channel — local MCP server exposing project tools to a regular GPT-5.5
    /// (native tool-calling; reach it from ChatGPT via a public tunnel).
    Mcp(McpArgs),
    /// Executor handoff — feed a delegation packet to Codex / Claude Code to run.
    Handoff(HandoffArgs),
}

/// Which local coding agent executes a handed-off plan.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Executor {
    Codex,
    ClaudeCode,
}

/// Flags shared by every mode that opens a ChatGPT web channel.
#[derive(Args, Debug, Clone)]
pub struct ChannelArgs {
    /// Which Chrome profile to drive: auto (default) | relay | "Profile 3".
    #[arg(long, default_value = "auto")]
    pub profile: String,
    /// chrome-use session name to reuse a tab group across runs.
    #[arg(long)]
    pub session: Option<String>,
    /// File the conversation under a ChatGPT Project (empty = plain chat).
    #[arg(long, default_value = "chatgpt-use")]
    pub project: String,
    /// Total wall-clock budget per model turn, in seconds.
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,
    /// Select the browser-channel model: pro | thinking | instant | <raw label>.
    /// GPT-5.5 Pro can only be reached here (it has no Apps/MCP). Default: account default.
    #[arg(long)]
    pub model: Option<String>,
}

#[derive(Args, Debug)]
pub struct AskArgs {
    /// The question / instruction to send to ChatGPT.
    pub prompt: String,
    /// Files whose contents are prepended as context (repeatable).
    #[arg(long = "file")]
    pub files: Vec<String>,
    /// Delegation mode: ask (plain text) | plan | review | debug | research.
    /// Non-ask modes send a typed delegation packet and parse a structured reply.
    #[arg(long, value_enum, default_value_t = crate::delegation::Mode::Ask)]
    pub mode: crate::delegation::Mode,
    /// For non-ask modes, emit the parsed delegation packet as JSON on stdout.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub channel: ChannelArgs,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// The task for ChatGPT to accomplish by calling local tools.
    pub task: String,
    /// Working directory the tools operate in (default: current dir).
    #[arg(long)]
    pub cwd: Option<String>,
    /// Require interactive approval before each side-effecting tool call.
    #[arg(long)]
    pub approve: bool,
    /// Hard cap on agent-loop iterations.
    #[arg(long, default_value_t = 40)]
    pub max_steps: u32,
    #[command(flatten)]
    pub channel: ChannelArgs,
}

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Port for the local Anthropic-compatible endpoint.
    #[arg(long, default_value_t = 8787)]
    pub port: u16,
    /// Bind host.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    #[command(flatten)]
    pub channel: ChannelArgs,
}

#[derive(Args, Debug)]
pub struct McpArgs {
    /// Port for the local MCP server (expose via a public tunnel for ChatGPT).
    #[arg(long, default_value_t = 8788)]
    pub port: u16,
    /// Bind host.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Root directory the exposed tools operate in (default: current dir).
    #[arg(long)]
    pub cwd: Option<String>,
    /// Shared secret required to call the server (recommended when tunneled).
    #[arg(long)]
    pub token: Option<String>,
}

#[derive(Args, Debug)]
pub struct HandoffArgs {
    /// Path to a delegation-packet JSON file, or "-" to read it from stdin.
    pub packet: String,
    /// Which local agent runs the plan.
    #[arg(long, value_enum, default_value_t = Executor::Codex)]
    pub to: Executor,
    /// Working directory the executor runs in (default: current dir).
    #[arg(long)]
    pub cwd: Option<String>,
    /// Actually launch the executor. Without it, print the assembled command (dry-run).
    #[arg(long)]
    pub execute: bool,
}
