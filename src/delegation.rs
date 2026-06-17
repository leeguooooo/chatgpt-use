//! Structured delegation — the proven, Pro-compatible main line (borrowed from
//! RPG-478/codex-chatgpt-bridge). Instead of making web ChatGPT role-play tools,
//! we gather context locally, hand ChatGPT a typed DELEGATION PACKET prompt
//! (sender = a machine, not a human), and parse a strict structured reply with a
//! machine-readable `verdict`. The executor (Codex / Claude Code) only ever
//! consumes packets; web ChatGPT never edits files directly.
//!
//! PUBLIC TYPES ARE FROZEN (Phase 0). The `delegation`/`ask` agent implements the
//! fn bodies; the `handoff` agent consumes DelegationPacket. Do not change the
//! type shapes or signatures.

use anyhow::Result;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// The delegation mode — shapes the prompt and the expected reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Plain Q&A — return free text (current Mode 1 behavior, no packet).
    Ask,
    /// Produce an implementation plan packet.
    Plan,
    /// Review provided context and return a verdict + findings.
    Review,
    /// Diagnose a bug and propose a fix plan.
    Debug,
    /// Research a question and return a sourced summary.
    Research,
}

/// Whether the executor should act on the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Proceed,
    Revise,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: u32,
    pub action: String,
    pub target: String,
    pub success_criteria: String,
}

/// The structured packet ChatGPT returns and the executor consumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationPacket {
    pub goal: String,
    #[serde(default)]
    pub summary: Vec<String>,
    #[serde(default)]
    pub plan: Vec<PlanStep>,
    #[serde(default)]
    pub risks: Vec<String>,
    #[serde(default)]
    pub tests: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub do_not_do: Vec<String>,
    pub verdict: Verdict,
}

/// Build the delegation-packet prompt for `mode`, embedding the compact context.
///
/// Frame it as a MACHINE delegation (sender is an automated controller, not a
/// human) and require ONLY a single fenced ```json block matching DelegationPacket.
pub fn build_prompt(_mode: Mode, _task: &str, _context: &str) -> String {
    todo!("delegation agent: render mode-typed delegation-packet prompt")
}

/// Parse + validate ChatGPT's reply into a DelegationPacket. Fail fast (Err) on a
/// missing/empty verdict or unparseable block — never return an ambiguous packet.
pub fn parse_packet(_reply: &str) -> Result<DelegationPacket> {
    todo!("delegation agent: extract fenced json, validate verdict, fail-fast")
}
