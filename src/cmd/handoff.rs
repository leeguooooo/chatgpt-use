//! Executor handoff — take a structured delegation packet (the plan ChatGPT
//! produced via `ask --mode plan`) and hand it to a local coding agent to run.
//! This closes the loop: ChatGPT (esp. Pro) plans; Codex / Claude Code executes;
//! web ChatGPT never edits files directly.
//!
//! Read the packet from `args.packet` (a file path, or "-" for stdin), parse it
//! with `crate::delegation::DelegationPacket` (serde_json). If `verdict` is not
//! `Proceed`, refuse to run and explain (Revise → needs another planning pass;
//! Blocked → surface the risks). Otherwise render the packet into a single
//! executor instruction string (goal + numbered plan + acceptance + do_not_do +
//! tests) and:
//!   - Codex       → `codex exec "<instruction>"` (or the project's codex CLI)
//!   - ClaudeCode  → `claude -p "<instruction>"`
//! With `--execute`, spawn the chosen executor (std::process::Command) in `cwd`
//! and stream its output. Without it, DRY-RUN: print the assembled command and
//! the instruction so the user can inspect before running.
//!
//! Owned by the HANDOFF agent.

use crate::cli::HandoffArgs;
use anyhow::Result;

pub fn run(_args: &HandoffArgs) -> Result<()> {
    todo!("HANDOFF: parse DelegationPacket, gate on verdict, render + run executor")
}
