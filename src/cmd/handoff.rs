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

use crate::cli::{Executor, HandoffArgs};
use crate::delegation::{DelegationPacket, Verdict};
use anyhow::{anyhow, Context, Result};
use std::io::Read as _;

/// Read raw packet bytes: "-" means stdin, anything else is a file path.
fn read_packet_source(packet: &str) -> Result<String> {
    if packet == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read delegation packet from stdin")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(packet)
            .with_context(|| format!("failed to read delegation packet from '{packet}'"))
    }
}

/// Parse JSON into a `DelegationPacket`, giving a clear error on failure.
fn parse_packet(raw: &str) -> Result<DelegationPacket> {
    serde_json::from_str(raw).context(
        "delegation packet is not valid JSON or does not match DelegationPacket schema",
    )
}

/// Gate on `verdict`: only `Proceed` passes; `Revise` / `Blocked` return `Err`.
fn check_verdict(packet: &DelegationPacket) -> Result<()> {
    match packet.verdict {
        Verdict::Proceed => Ok(()),
        Verdict::Revise => {
            let risks = if packet.risks.is_empty() {
                "(no risks listed)".to_owned()
            } else {
                packet.risks.join(", ")
            };
            Err(anyhow!(
                "plan verdict is REVISE — another planning pass is needed before execution.\n\
                 Risks surfaced: {risks}"
            ))
        }
        Verdict::Blocked => {
            let blockers = if packet.risks.is_empty() {
                "(no blockers listed)".to_owned()
            } else {
                packet.risks.join(", ")
            };
            Err(anyhow!(
                "plan verdict is BLOCKED — execution refused.\n\
                 Blockers: {blockers}"
            ))
        }
    }
}

/// Render a `DelegationPacket` into a single, self-contained instruction string
/// that an executor (Codex / Claude Code) can act on without additional context.
///
/// Pure function — no I/O, easy to test.
pub(crate) fn render_instruction(packet: &DelegationPacket) -> String {
    let mut out = String::new();

    // Goal
    out.push_str("GOAL\n");
    out.push_str(&packet.goal);
    out.push_str("\n\n");

    // Summary (optional)
    if !packet.summary.is_empty() {
        out.push_str("SUMMARY\n");
        for line in &packet.summary {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }

    // Numbered plan
    if !packet.plan.is_empty() {
        out.push_str("PLAN\n");
        for step in &packet.plan {
            out.push_str(&format!(
                "{}. {} → {} [{}]\n",
                step.step, step.action, step.target, step.success_criteria
            ));
        }
        out.push('\n');
    }

    // Acceptance criteria
    if !packet.acceptance.is_empty() {
        out.push_str("ACCEPTANCE CRITERIA\n");
        for criterion in &packet.acceptance {
            out.push_str("- ");
            out.push_str(criterion);
            out.push('\n');
        }
        out.push('\n');
    }

    // Tests to run
    if !packet.tests.is_empty() {
        out.push_str("TESTS TO RUN\n");
        for test in &packet.tests {
            out.push_str("- ");
            out.push_str(test);
            out.push('\n');
        }
        out.push('\n');
    }

    // Explicit prohibitions
    if !packet.do_not_do.is_empty() {
        out.push_str("DO NOT:\n");
        for item in &packet.do_not_do {
            out.push_str("- ");
            out.push_str(item);
            out.push('\n');
        }
        out.push('\n');
    }

    // Trim trailing whitespace
    out.trim_end().to_owned()
}

/// Build the `std::process::Command` for the chosen executor, ready to spawn.
/// The instruction is passed as a single argument (no shell interpolation).
fn build_command(executor: Executor, instruction: &str, cwd: &std::path::Path) -> std::process::Command {
    let (program, subcommand) = match executor {
        Executor::Codex => ("codex", Some("exec")),
        Executor::ClaudeCode => ("claude", None),
    };

    let mut cmd = std::process::Command::new(program);
    cmd.current_dir(cwd);

    // Codex: `codex exec "<instruction>"`
    // ClaudeCode: `claude -p "<instruction>"`
    match executor {
        Executor::Codex => {
            cmd.arg(subcommand.unwrap());
            cmd.arg(instruction);
        }
        Executor::ClaudeCode => {
            cmd.arg("-p");
            cmd.arg(instruction);
        }
    }

    cmd
}

/// Format the executor command line as a human-readable string for dry-run output.
fn format_command_line(executor: Executor, instruction: &str) -> String {
    match executor {
        Executor::Codex => format!("codex exec {:?}", instruction),
        Executor::ClaudeCode => format!("claude -p {:?}", instruction),
    }
}

pub fn run(args: &HandoffArgs) -> Result<()> {
    // 1. Read and parse the packet.
    let raw = read_packet_source(&args.packet)?;
    let packet = parse_packet(&raw)?;

    // 2. Gate on verdict.
    check_verdict(&packet)?;

    // 3. Render the instruction string.
    let instruction = render_instruction(&packet);

    // 4. Resolve the working directory.
    let cwd = match &args.cwd {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::env::current_dir().context("could not determine current directory")?,
    };

    let executor_label = match args.to {
        Executor::Codex => "Codex",
        Executor::ClaudeCode => "Claude Code",
    };

    // 5. Execute or dry-run.
    if args.execute {
        println!("=== HANDOFF: executing via {executor_label} ===");
        println!("Working directory: {}", cwd.display());
        println!("--- instruction ---");
        println!("{instruction}");
        println!("--- end instruction ---\n");

        let mut cmd = build_command(args.to, &instruction, &cwd);
        cmd.stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());

        let status = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {executor_label}"))?
            .wait()
            .with_context(|| format!("failed to wait for {executor_label}"))?;

        if !status.success() {
            let code = status.code().unwrap_or(1);
            return Err(anyhow!("{executor_label} exited with status {code}"));
        }
    } else {
        // Dry-run: show the assembled command + instruction.
        println!("=== DRY-RUN: executor handoff (pass --execute to actually run) ===");
        println!("Executor  : {executor_label}");
        println!("Directory : {}", cwd.display());
        println!("Command   : {}", format_command_line(args.to, &instruction));
        println!();
        println!("--- instruction that would be sent ---");
        println!("{instruction}");
        println!("--- end instruction ---");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::{DelegationPacket, PlanStep, Verdict};

    fn sample_packet(verdict: Verdict) -> DelegationPacket {
        DelegationPacket {
            goal: "Implement the executor handoff module.".to_owned(),
            summary: vec!["Parse a delegation packet and spawn an executor.".to_owned()],
            plan: vec![
                PlanStep {
                    step: 1,
                    action: "Create handoff.rs".to_owned(),
                    target: "src/cmd/handoff.rs".to_owned(),
                    success_criteria: "file compiles without warnings".to_owned(),
                },
                PlanStep {
                    step: 2,
                    action: "Add unit tests".to_owned(),
                    target: "src/cmd/handoff.rs #[cfg(test)]".to_owned(),
                    success_criteria: "cargo test passes".to_owned(),
                },
            ],
            risks: vec!["Sandbox blocks git writes".to_owned()],
            tests: vec!["cargo test 2>&1 | tail -40".to_owned()],
            acceptance: vec!["All tests pass".to_owned(), "No compiler warnings".to_owned()],
            do_not_do: vec![
                "Do not edit Cargo.toml".to_owned(),
                "Do not commit or push".to_owned(),
            ],
            verdict,
        }
    }

    /// A Proceed packet must render goal, numbered steps, and do_not_do.
    #[test]
    fn test_render_instruction_proceed() {
        let packet = sample_packet(Verdict::Proceed);
        let rendered = render_instruction(&packet);

        // Goal section
        assert!(rendered.contains("GOAL"), "missing GOAL section");
        assert!(
            rendered.contains("Implement the executor handoff module."),
            "missing goal text"
        );

        // Plan section with numbering
        assert!(rendered.contains("PLAN"), "missing PLAN section");
        assert!(
            rendered.contains("1. Create handoff.rs → src/cmd/handoff.rs"),
            "missing step 1"
        );
        assert!(
            rendered.contains("2. Add unit tests"),
            "missing step 2"
        );

        // Acceptance criteria
        assert!(
            rendered.contains("ACCEPTANCE CRITERIA"),
            "missing ACCEPTANCE section"
        );
        assert!(rendered.contains("All tests pass"), "missing acceptance item");

        // Do-not-do list
        assert!(rendered.contains("DO NOT:"), "missing DO NOT section");
        assert!(
            rendered.contains("Do not edit Cargo.toml"),
            "missing do_not_do item"
        );
        assert!(
            rendered.contains("Do not commit or push"),
            "missing second do_not_do item"
        );
    }

    /// Verdict::Blocked must return Err, not Ok.
    #[test]
    fn test_verdict_blocked_returns_err() {
        let packet = sample_packet(Verdict::Blocked);
        let result = check_verdict(&packet);
        assert!(result.is_err(), "Blocked verdict should return Err");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("BLOCKED"), "error message should mention BLOCKED");
    }

    /// Verdict::Revise must return Err with the right message.
    #[test]
    fn test_verdict_revise_returns_err() {
        let packet = sample_packet(Verdict::Revise);
        let result = check_verdict(&packet);
        assert!(result.is_err(), "Revise verdict should return Err");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("REVISE"), "error message should mention REVISE");
        assert!(
            msg.contains("Sandbox blocks git writes"),
            "error should surface the risks"
        );
    }

    /// Verdict::Proceed must return Ok.
    #[test]
    fn test_verdict_proceed_is_ok() {
        let packet = sample_packet(Verdict::Proceed);
        assert!(check_verdict(&packet).is_ok(), "Proceed should return Ok");
    }

    /// A packet with no optional fields still renders a coherent instruction.
    #[test]
    fn test_render_instruction_minimal() {
        let packet = DelegationPacket {
            goal: "Minimal goal.".to_owned(),
            summary: vec![],
            plan: vec![],
            risks: vec![],
            tests: vec![],
            acceptance: vec![],
            do_not_do: vec![],
            verdict: Verdict::Proceed,
        };
        let rendered = render_instruction(&packet);
        assert!(rendered.contains("Minimal goal."), "goal should always appear");
        // Absent sections should not appear as empty noise
        assert!(!rendered.contains("PLAN\n\n"), "empty plan should be omitted");
        assert!(!rendered.contains("DO NOT:"), "empty do_not_do should be omitted");
    }
}
