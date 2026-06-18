//! chatgpt-use — drive a logged-in ChatGPT web subscription as a coding-agent
//! backend via `chrome-use`. Three modes share one engine (see README):
//!   ask   — Mode 1 (sidekick): one-shot question, no tools
//!   run   — Mode 2 (brain): ChatGPT drives local tools in an agent loop
//!   serve — Mode 3 (drop-in): Anthropic-compatible shim for Claude Code
//!
//! main.rs owns the CLI surface and module wiring ONLY. Implementation lives in
//! the leaf modules below, each independently owned so they never collide.

mod channel; // chrome-use-driven transport to the ChatGPT web conversation
mod cli; // clap argument definitions
mod cmd; // subcommand entry points (ask / run / serve / mcp / handoff)
mod delegation; // structured delegation packets (the planner/reviewer main line)
mod ledger; // append-only audit trail at ~/.chatgpt-use/ledger.jsonl
mod protocol; // tool-call text protocol: types, system prompt, parsing, rendering
mod tools; // local tool executor (read_file / write_file / bash / grep / list_dir)

use clap::Parser;
use cli::{Cli, Command};

fn main() {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Ask(args) => cmd::ask::run(args),
        Command::Run(args) => cmd::run::run(args),
        Command::Serve(args) => cmd::serve::run(args),
        Command::Mcp(args) => cmd::mcp::run(args),
        Command::Handoff(args) => cmd::handoff::run(args),
        Command::Init(args) => cmd::init::run(args),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
