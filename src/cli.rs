//! CLI argument definitions (clap). Owned by the orchestrator; the cmd modules
//! consume these structs but do not modify this file.

use clap::{Args, Parser, Subcommand};

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
}

#[derive(Args, Debug)]
pub struct AskArgs {
    /// The question / instruction to send to ChatGPT.
    pub prompt: String,
    /// Files whose contents are prepended as context (repeatable).
    #[arg(long = "file")]
    pub files: Vec<String>,
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
