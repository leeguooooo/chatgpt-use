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
    /// One-time setup — generate an auth token at ~/.chatgpt-use/auth.json and
    /// print the next steps (the `mcp` command auto-loads it when --token is omitted).
    Init(InitArgs),
    /// Closed loop — dispatch a task to ChatGPT which DOES it on the local project
    /// via its chatgpt-use MCP connector (read/build/test/logs), then reports back.
    /// Requires the connector connected + an `mcp --profile full` server running.
    Work(WorkArgs),
    /// Refresh the chatgpt-use connector in ChatGPT settings (re-runs tools/list).
    /// Run this after restarting the `mcp` server so ChatGPT re-discovers the tools.
    Refresh(RefreshArgs),
}

#[derive(Args, Debug)]
pub struct WorkArgs {
    /// The task for ChatGPT to carry out on the local project via its connector tools.
    pub task: String,
    /// If ChatGPT replies without evidence it actually ran the tools (a thin or
    /// hedging report), re-nudge it this many extra times in the same conversation.
    #[arg(long, default_value_t = 1)]
    pub retries: u32,
    /// Keep the task going across turns: ChatGPT ends each report with
    /// `STATUS: DONE` or `STATUS: CONTINUE`, and we auto-send "continue" until it
    /// says DONE or `--max-turns` is hit. Lets one task span many tool steps.
    #[arg(long)]
    pub r#loop: bool,
    /// Hard cap on turns when --loop is set (each turn is one model reply).
    #[arg(long, default_value_t = 8)]
    pub max_turns: u32,
    #[command(flatten)]
    pub channel: ChannelArgs,
}

#[derive(Args, Debug)]
pub struct RefreshArgs {
    /// Display name of the connector to refresh in ChatGPT settings.
    #[arg(long, default_value = "chatgpt-use")]
    pub connector: String,
    /// Override the settings URL to open (default: the Connectors settings page).
    #[arg(long)]
    pub url: Option<String>,
    #[command(flatten)]
    pub channel: ChannelArgs,
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Overwrite an existing token.
    #[arg(long)]
    pub force: bool,
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
    /// Select the composer "Intelligence" level: instant | medium | high |
    /// "extra high" | pro (or a raw menu label). GPT-5.5 Pro can only be reached
    /// here (it has no Apps/MCP). Default: the account's current level.
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
    /// Command-gating level for `bash` (safe|trusted|dangerous). Local Mode-2
    /// defaults to trusted.
    #[arg(long, value_enum, default_value_t = PermissionMode::Trusted)]
    pub permission_mode: PermissionMode,
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
    /// Which tools to expose: read-only (read_file/list_dir/grep — DEFAULT, safe
    /// for a public tunnel) or full (also write_file/bash — only on a trusted,
    /// non-exposed setup).
    #[arg(long, value_enum, default_value_t = ToolProfile::ReadOnly)]
    pub profile: ToolProfile,
    /// Command-gating level for `bash` under --profile full (safe|trusted|dangerous).
    #[arg(long, value_enum, default_value_t = PermissionMode::Safe)]
    pub permission_mode: PermissionMode,
    /// Authentication mode: token (default shared-secret Bearer) or oauth
    /// (OAuth 2.1 Authorization-Code + PKCE for ChatGPT "OAuth" connector mode).
    #[arg(long, value_enum, default_value_t = AuthMode::Token)]
    pub auth_mode: AuthMode,
    /// Per-command timeout (seconds) for the persistent `bash` terminal under
    /// --profile full; 0 = unlimited. Bounds a hung command so it can't freeze
    /// the single-threaded server. The shell keeps cwd + exported env across calls.
    #[arg(long, default_value_t = 300)]
    pub bash_timeout: u64,
}

/// Authentication mode for the MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuthMode {
    /// Shared-secret Bearer token (default). Pass `--token` or use `chatgpt-use init`.
    Token,
    /// OAuth 2.1 Authorization-Code + PKCE (S256). ChatGPT "OAuth" connector mode.
    #[value(name = "oauth", alias = "o-auth")]
    OAuth,
}

/// Tool-exposure profile for the MCP channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ToolProfile {
    /// read_file / list_dir / grep + git_* only — safe to expose over a tunnel.
    ReadOnly,
    /// All tools incl. write_file / edit_file / bash — trusted/local use only.
    Full,
}

/// How aggressively to gate side-effecting commands (`bash`). Borrowed from
/// coding-tools-mcp's permission modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PermissionMode {
    /// Block destructive, network, and shell-substitution commands; filter
    /// secret-looking env vars. The default.
    Safe,
    /// Allow local-dev commands (incl. network); still filters secret env vars.
    Trusted,
    /// No gates at all. Only on a fully trusted machine.
    Dangerous,
}

#[derive(Args, Debug)]
pub struct HandoffArgs {
    /// Path to a delegation-packet JSON file, or "-" to read it from stdin.
    pub packet: String,
    /// Which local agent runs the plan.
    #[arg(long, value_enum, required = true)]
    pub to: Executor,
    /// Working directory the executor runs in (default: current dir).
    #[arg(long)]
    pub cwd: Option<String>,
    /// Actually launch the executor. Without it, print the assembled command (dry-run).
    #[arg(long)]
    pub execute: bool,
}
