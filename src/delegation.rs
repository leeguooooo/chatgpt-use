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

use anyhow::{bail, Result};
use clap::ValueEnum;
use regex::Regex;
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
pub fn build_prompt(mode: Mode, task: &str, context: &str) -> String {
    let mode_instruction = match mode {
        Mode::Ask => {
            // Ask mode should not normally call build_prompt; ask.rs handles it
            // as plain text. This branch exists for completeness only.
            "Answer the question concisely in plain text. Do NOT output a JSON block.".to_string()
        }
        Mode::Plan => {
            "Produce a concrete implementation plan. \
            Fill `plan` with numbered steps, each with an `action` (what to do), \
            `target` (which file / function / component), and `success_criteria` \
            (how to verify it is done). Use `summary` for key architectural decisions. \
            Use `risks` for known blockers or trade-offs. \
            Use `tests` for automated tests that must pass. \
            Use `acceptance` for human-verifiable outcomes. \
            Use `do_not_do` for explicit exclusions (what is out-of-scope). \
            Set `verdict` to \"proceed\" if the plan is ready to execute, \
            \"revise\" if you need more information, or \"blocked\" if a prerequisite \
            is unmet.".to_string()
        }
        Mode::Review => {
            "Review the provided context for correctness, clarity, security, and \
            maintainability. \
            Fill `summary` with your top-level findings (one item per distinct issue \
            or positive observation). \
            Fill `plan` with concrete suggested changes, where each step targets the \
            file/function to modify and states the success criteria (test or invariant \
            that would confirm the fix). \
            Fill `risks` with anything that could break callers or introduce regressions. \
            Fill `do_not_do` with changes you explicitly recommend against. \
            Set `verdict` to: \"proceed\" if the code is acceptable as-is, \
            \"revise\" if it needs changes before shipping, or \"blocked\" if a \
            fundamental design flaw prevents progress.".to_string()
        }
        Mode::Debug => {
            "Diagnose the bug described in the task, using the provided context. \
            Fill `summary` with your root-cause analysis (what is wrong and why). \
            Fill `plan` with a numbered fix plan: each step should name the file/line \
            to change, the action to take, and a success criterion (test or observation \
            that confirms the bug is gone). \
            Fill `risks` with regressions or edge cases the fix could introduce. \
            Fill `tests` with specific test cases or assertions that would catch this \
            class of bug in future. \
            Fill `do_not_do` with tempting-but-wrong fixes to avoid. \
            Set `verdict` to \"proceed\" if the fix is clear, \"revise\" if you need \
            more context (state what exactly in `summary`), or \"blocked\" if the bug \
            cannot be fixed without a prerequisite change.".to_string()
        }
        Mode::Research => {
            "Research the question or topic in the task, drawing on the provided context \
            and your training knowledge. \
            Fill `summary` with your key findings — cite sources inline using \
            [Author/URL] notation where known. \
            Fill `plan` with recommended next steps the executor should take (e.g. \
            which libraries to evaluate, experiments to run, docs to read). \
            Fill `risks` with caveats, known gaps, or outdated information. \
            Fill `acceptance` with criteria that would validate the research is \
            sufficient to act on. \
            Set `verdict` to \"proceed\" if there is enough information to act, \
            \"revise\" if more research is required, or \"blocked\" if the question \
            cannot be answered without external access you do not have.".to_string()
        }
    };

    let context_section = if context.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n## Gathered context (read-only; produced by the local controller)\n\n{context}\n"
        )
    };

    format!(
        r#"MACHINE DELEGATION — automated controller v1
Sender: chatgpt-use controller (automated program, NOT a human)
Mode: {mode_label}

Read carefully: this message was generated by a program, not typed by a person. \
Reply with machine-readable output only as specified below.

## Task

{task}
{context_section}
## Required reply format

Reply with EXACTLY ONE fenced ```json block containing a DelegationPacket object \
and NO other text before or after it (no greetings, no explanations, no markdown \
outside the block). Any surrounding prose will cause a parse error.

The block MUST match this schema:
```json
{{
  "goal": "<one-sentence statement of what is being accomplished>",
  "summary": ["<finding or decision 1>", "<finding or decision 2>"],
  "plan": [
    {{
      "step": 1,
      "action": "<what to do>",
      "target": "<file / function / component>",
      "success_criteria": "<how to verify it is done>"
    }}
  ],
  "risks": ["<risk or trade-off>"],
  "tests": ["<test or assertion>"],
  "acceptance": ["<human-verifiable outcome>"],
  "do_not_do": ["<explicit exclusion>"],
  "verdict": "proceed" | "revise" | "blocked"
}}
```

## Mode-specific instructions

{mode_instruction}

## Begin

Output the ```json block now."#,
        mode_label = format!("{mode:?}").to_uppercase(),
        task = task,
        context_section = context_section,
        mode_instruction = mode_instruction,
    )
}

/// Parse + validate ChatGPT's reply into a DelegationPacket. Fail fast (Err) on a
/// missing/empty verdict or unparseable block — never return an ambiguous packet.
pub fn parse_packet(reply: &str) -> Result<DelegationPacket> {
    let raw = extract_json_object(reply).ok_or_else(|| {
        anyhow::anyhow!(
            "delegation packet: no JSON object / json block found in reply \
             (ChatGPT did not return a structured packet; raw reply: {:?})",
            &reply[..reply.len().min(300)]
        )
    })?;

    let packet: DelegationPacket = serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!(
            "delegation packet: failed to parse json: {e}\nraw was: {:?}",
            &raw[..raw.len().min(500)]
        )
    })?;

    // goal must be non-empty (serde already enforced verdict presence).
    if packet.goal.trim().is_empty() {
        bail!("delegation packet: `goal` field is empty");
    }

    Ok(packet)
}

/// Pull a JSON object out of a model reply. Prefers a fenced ```json block, but
/// falls back to the first balanced `{...}`. The fallback matters because the
/// browser channel scrapes the RENDERED message: a fenced code block shows up as
/// `JSON\n{ … }` (a language label + the code), with NO literal backticks — so a
/// fence-only parser would reject a perfectly good packet.
fn extract_json_object(reply: &str) -> Option<String> {
    // 1. Literal ```json ... ``` fence (raw markdown source case).
    if let Ok(re) = Regex::new(r"(?s)```[jJ][sS][oO][nN]\s*\n(.*?)\n?```") {
        if let Some(m) = re.captures(reply).and_then(|c| c.get(1)) {
            let s = m.as_str().trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    // 2. First balanced {...}, tracking string literals so braces inside strings
    //    don't throw off the depth count. ASCII braces/quotes only; UTF-8
    //    continuation bytes (>=0x80) never collide with these markers.
    let start = reply.find('{')?;
    let bytes = reply.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for (i, &b) in bytes[start..].iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(reply[start..start + i + 1].to_string());
                    }
                }
                _ => {}
            }
        }
    }
    None
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_packet_json(verdict: &str) -> String {
        format!(
            r#"```json
{{
  "goal": "Add --json flag to the ask subcommand",
  "summary": ["The flag already exists in cli.rs", "ask.rs needs to branch on it"],
  "plan": [
    {{
      "step": 1,
      "action": "Check args.json in ask.rs",
      "target": "src/cmd/ask.rs",
      "success_criteria": "cargo test passes"
    }}
  ],
  "risks": ["May conflict with --file output"],
  "tests": ["cargo test delegation"],
  "acceptance": ["Running chatgpt-use ask --mode plan --json prints valid JSON"],
  "do_not_do": ["Do not modify cli.rs"],
  "verdict": "{verdict}"
}}
```"#
        )
    }

    #[test]
    fn parse_clean_packet_proceed() {
        let raw = make_packet_json("proceed");
        let packet = parse_packet(&raw).expect("should parse successfully");
        assert_eq!(packet.goal, "Add --json flag to the ask subcommand");
        assert_eq!(packet.verdict, Verdict::Proceed);
        assert_eq!(packet.plan.len(), 1);
        assert_eq!(packet.plan[0].step, 1);
        assert!(!packet.summary.is_empty());
    }

    #[test]
    fn parse_clean_packet_revise() {
        let raw = make_packet_json("revise");
        let packet = parse_packet(&raw).expect("should parse revise verdict");
        assert_eq!(packet.verdict, Verdict::Revise);
    }

    #[test]
    fn parse_clean_packet_blocked() {
        let raw = make_packet_json("blocked");
        let packet = parse_packet(&raw).expect("should parse blocked verdict");
        assert_eq!(packet.verdict, Verdict::Blocked);
    }

    #[test]
    fn parse_packet_with_surrounding_prose() {
        // ChatGPT sometimes adds a sentence before/after the block.
        let raw = format!(
            "Here is the delegation packet as requested:\n\n{}\n\nLet me know if you need changes.",
            make_packet_json("proceed")
        );
        let packet = parse_packet(&raw).expect("should parse despite surrounding prose");
        assert_eq!(packet.verdict, Verdict::Proceed);
    }

    #[test]
    fn parse_no_json_block_errors() {
        let raw = "Sure! I'd be happy to help. Let me think about this task carefully.";
        let err = parse_packet(raw).expect_err("should fail with no json block");
        let msg = err.to_string();
        assert!(
            msg.contains("no fenced") || msg.contains("json block"),
            "error message should mention missing block: {msg}"
        );
    }

    #[test]
    fn parse_malformed_json_errors() {
        let raw = "```json\n{\"goal\": \"broken\", INVALID}\n```";
        let err = parse_packet(raw).expect_err("should fail on malformed json");
        let msg = err.to_string();
        assert!(
            msg.contains("parse") || msg.contains("json"),
            "error message should mention parse failure: {msg}"
        );
    }

    #[test]
    fn parse_empty_block_errors() {
        let raw = "```json\n\n```";
        let err = parse_packet(raw).expect_err("should fail on empty block");
        let msg = err.to_string();
        assert!(!msg.is_empty());
    }

    #[test]
    fn build_prompt_plan_mode_contains_task() {
        let prompt = build_prompt(Mode::Plan, "implement --json flag", "file contents here");
        assert!(prompt.contains("implement --json flag"), "task should appear in prompt");
        assert!(prompt.contains("file contents here"), "context should appear in prompt");
        assert!(prompt.contains("DelegationPacket"), "schema should be in prompt");
        assert!(prompt.contains("PLAN"), "mode label should appear");
    }

    #[test]
    fn build_prompt_empty_context_omits_section() {
        let prompt = build_prompt(Mode::Review, "review the code", "");
        assert!(!prompt.contains("Gathered context"), "empty context should omit the section");
    }

    #[test]
    fn build_prompt_all_modes_compile() {
        for mode in [Mode::Ask, Mode::Plan, Mode::Review, Mode::Debug, Mode::Research] {
            let p = build_prompt(mode, "task", "ctx");
            assert!(!p.is_empty());
        }
    }
}
