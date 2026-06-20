//! `chatgpt-use work "<task>"` — the closed loop. Claude Code (or any caller)
//! dispatches a task; ChatGPT carries it out ON THE LOCAL PROJECT using its
//! `chatgpt-use` MCP connector tools (read_file / grep / git_* / write_file /
//! edit_file / bash — build, run tests, read logs), then reports the result,
//! which we scrape back to the caller.
//!
//! This is NOT the browser text-tool-protocol (that hits the role-play wall):
//! the tools are NATIVE MCP tools the connector gives ChatGPT, so it really
//! calls them (server-side, out of band). We just drive the conversation and
//! read the final report.
//!
//! Two robustness features on top of a single send:
//!   * --retries: ChatGPT-Instant is run-to-run flaky — sometimes it hedges
//!     ("I can do that, just point me at the file") instead of calling a tool.
//!     We detect a thin/hedging report and re-nudge in the SAME conversation
//!     (so the context is retained) before giving up.
//!   * --loop: a big task can't finish in one turn. We ask ChatGPT to end each
//!     report with `STATUS: DONE` or `STATUS: CONTINUE` and auto-send "continue"
//!     until it says DONE (or --max-turns is hit), accumulating the reports.
//!
//! Prereqs: the `chatgpt-use` connector is connected in ChatGPT, and a
//! `chatgpt-use mcp --profile full` server (+ tunnel) is running so ChatGPT can
//! build/run. The connector only works on a NON-Pro model, so we default to the
//! Instant level.

use crate::channel::{Channel, ChannelOptions, SendOptions};
use crate::cli::WorkArgs;
use anyhow::Result;

/// The sentinel ChatGPT must append so --loop can tell whether the task is done.
const STATUS_LINE: &str =
    "End your reply with a line that is exactly `STATUS: DONE` if the task is fully \
     complete, or `STATUS: CONTINUE` if more tool steps remain.";

pub fn run(args: &WorkArgs) -> Result<()> {
    // The connector requires a non-Pro model; default to Instant unless the
    // caller explicitly picked a level.
    let model = args
        .channel
        .model
        .clone()
        .or_else(|| Some("instant".to_string()));

    // Building/testing can take minutes; give it plenty of room.
    let timeout_secs = args.channel.timeout.max(1200);

    let opts = ChannelOptions {
        profile: args.channel.profile.clone(),
        session: args.channel.session.clone(),
        project: args.channel.project.clone(),
        timeout_secs,
        model,
    };

    let sopts = SendOptions::work();
    let mut channel = Channel::connect(&opts)?;

    // Run the dispatch (+ thin-report retries), then optionally keep the loop
    // going across turns. close() no matter how it ends.
    let result = drive(&mut channel, args, &sopts);
    channel.close();
    let text = result?;

    crate::ledger::record(
        "work",
        serde_json::json!({
            "task_chars": args.task.len(),
            "reply_chars": text.len(),
            "looped": args.r#loop,
        }),
    );
    println!("{text}");
    Ok(())
}

/// Send the task, retry on a thin report, then loop "continue" turns if asked.
/// Returns the accumulated report text.
fn drive(channel: &mut Channel, args: &WorkArgs, sopts: &SendOptions) -> Result<String> {
    let want_loop = args.r#loop;

    // Imperative framing: a soft prompt makes the model hedge ("I'll first
    // connect the tools…") instead of calling them. Tell it to act NOW and not
    // ask for clarification — verified to trigger real connector tool calls.
    let mut prompt = format!(
        "TASK: {}\n\n\
         Carry out the TASK above RIGHT NOW on the local project using your connected `chatgpt-use` \
         tools — call them directly: read_file / list_dir / grep / git_status / git_diff / git_log / \
         git_show / git_blame / write_file / edit_file / bash (run builds/tests and read the real \
         output). Do NOT ask for clarification or for files, and do NOT just describe what you would \
         do — actually call the tools and do it. The task is fully specified above; if any detail is \
         ambiguous, make a reasonable choice and proceed. When finished, reply with a concise report: \
         the exact commands you ran, their key output, and the final result/status.",
        args.task
    );
    if want_loop {
        prompt.push_str("\n\n");
        prompt.push_str(STATUS_LINE);
    }

    // --- First turn (+ thin-report retries) -------------------------------
    let mut reply = send_until_substantive(channel, &prompt, args.retries, sopts)?;
    let mut transcript = reply.clone();

    if !want_loop {
        return Ok(transcript);
    }

    // --- Loop: keep nudging "continue" until DONE or --max-turns ----------
    let mut turn = 1u32;
    while turn < args.max_turns.max(1) {
        if status_is_done(&reply) {
            eprintln!("loop: ChatGPT reported STATUS: DONE after {turn} turn(s)");
            break;
        }
        turn += 1;
        eprintln!("loop: turn {turn}/{} — sending continue", args.max_turns);
        let cont = format!(
            "Continue the TASK — keep calling your `chatgpt-use` tools to make real progress, do \
             not repeat already-finished steps, and report what you did this turn. {STATUS_LINE}"
        );
        reply = send_until_substantive(channel, &cont, args.retries, sopts)?;
        transcript.push_str("\n\n---\n\n");
        transcript.push_str(&reply);
    }

    if turn >= args.max_turns.max(1) && !status_is_done(&reply) {
        eprintln!(
            "loop: hit --max-turns {} without STATUS: DONE — returning progress so far",
            args.max_turns
        );
    }
    Ok(transcript)
}

/// Send a message; if the reply looks thin/hedging (no sign ChatGPT ran a tool),
/// re-nudge in the same conversation up to `retries` extra times.
fn send_until_substantive(
    channel: &mut Channel,
    message: &str,
    retries: u32,
    sopts: &SendOptions,
) -> Result<String> {
    let mut reply = channel.send_with(message, sopts)?;
    let mut attempt = 0u32;
    while attempt < retries && is_thin_report(&reply) {
        attempt += 1;
        eprintln!(
            "report looks thin (no evidence of a real tool call) — re-nudging ({attempt}/{retries})"
        );
        let nudge = "That did not show any actual tool output. Call your `chatgpt-use` tools NOW \
                     (start with the one most relevant to the task), then report the REAL command \
                     output you got back — not a description of what you would do.";
        reply = channel.send_with(nudge, sopts)?;
    }
    Ok(reply)
}

/// True if `reply` ends with the DONE sentinel (case/space tolerant).
fn status_is_done(reply: &str) -> bool {
    reply
        .lines()
        .rev()
        .take(4)
        .any(|l| l.trim().eq_ignore_ascii_case("status: done"))
}

/// Heuristic: does the reply look like ChatGPT actually ran tools, or did it
/// hedge / describe without doing anything? We retry only on the latter.
///
/// Treat as THIN (retry) when the reply is very short, OR it leads with a
/// hedging/refusal phrase AND shows no concrete evidence of tool output (a code
/// block, a command, a file path, a git hash, etc.). Erring toward "substantive"
/// is safe — at worst we skip a retry; a false retry just wastes one turn.
fn is_thin_report(reply: &str) -> bool {
    let r = reply.trim();
    if r.len() < 40 {
        return true;
    }
    let lower = r.to_lowercase();

    // Positive evidence the model actually used a tool / produced real output.
    let has_evidence = r.contains("```")              // a code/output fence
        || lower.contains("$ ")                       // a shell prompt
        || r.contains("diff --git")
        || lower.contains("commit ")                  // git log/show output
        || regex_like_git_hash(r)
        || lower.contains("modified:")
        || lower.contains("error[")                   // compiler output
        || lower.contains("warning:")
        || r.lines().count() >= 8; // a long structured report is rarely pure hedging
    if has_evidence {
        return false;
    }

    // Hedging / refusal markers — present-tense "I would / I can / let me know /
    // could you / I don't have / unable / provide the".
    const HEDGE: &[&str] = &[
        "i would",
        "i can ",
        "i could",
        "let me know",
        "could you",
        "can you",
        "please provide",
        "provide the",
        "i don't have",
        "i do not have",
        "i'm unable",
        "i am unable",
        "i cannot access",
        "no access",
        "if you'd like",
        "would you like",
        "i'll need",
        "i would need",
        "to proceed",
        "happy to help",
    ];
    // CJK hedge/refusal markers (the model sometimes replies in Chinese):
    // 被拦截=blocked, 无法=cannot, 没有权限/受限=no permission/restricted,
    // 需要你/请提供=need you to/please provide, 我会继续尝试=I'll keep trying.
    const HEDGE_CJK: &[&str] = &[
        "被拦截", "拦截了", "无法访问", "无法执行", "没有权限", "受限",
        "需要你", "请提供", "请告诉我", "我会继续尝试", "未能执行", "无法完成",
    ];
    HEDGE.iter().any(|h| lower.contains(h)) || HEDGE_CJK.iter().any(|h| r.contains(h))
}

/// Cheap "looks like a 7–40 char hex git hash appears" check without pulling in
/// the regex crate at this layer.
fn regex_like_git_hash(s: &str) -> bool {
    s.split(|c: char| !c.is_ascii_alphanumeric()).any(|tok| {
        tok.len() >= 7 && tok.len() <= 40 && tok.chars().all(|c| c.is_ascii_hexdigit())
            && tok.chars().any(|c| c.is_ascii_digit())
            && tok.chars().any(|c| c.is_ascii_alphabetic())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_done_detected_at_tail() {
        assert!(status_is_done("did stuff\nSTATUS: DONE"));
        assert!(status_is_done("ran tests\n\nstatus: done\n"));
        assert!(!status_is_done("ran tests\nSTATUS: CONTINUE"));
        // DONE buried far above the tail should not count.
        let mut s = String::from("STATUS: DONE\n");
        for _ in 0..10 {
            s.push_str("more\n");
        }
        assert!(!status_is_done(&s));
    }

    #[test]
    fn thin_report_flags_hedging() {
        assert!(is_thin_report("Sure, I can do that. Could you provide the file path?"));
        assert!(is_thin_report("ok")); // too short
        assert!(is_thin_report(
            "I would start by reading the README, then I'd run the tests. Let me know if that works."
        ));
        // CJK confabulated-block / hedge with no real output → thin.
        assert!(is_thin_report(
            "第一条 uname -a 调用被工具层安全检查拦截了；我会继续尝试其余只读系统查询。"
        ));
    }

    #[test]
    fn thin_report_accepts_real_output() {
        assert!(!is_thin_report(
            "I ran `git log`:\n```\ncommit 316a388\nfeat: add work loop\n```\nDone."
        ));
        assert!(!is_thin_report(
            "git_status returned:\n modified: src/channel.rs\n modified: src/cli.rs"
        ));
        // A long structured report without hedging is substantive.
        let long = (0..10).map(|i| format!("step {i}: did a thing")).collect::<Vec<_>>().join("\n");
        assert!(!is_thin_report(&long));
    }

    #[test]
    fn git_hash_heuristic() {
        assert!(regex_like_git_hash("see commit 316a388 for details")); // mixed hex
        assert!(!regex_like_git_hash("the README file")); // no hash
        assert!(!regex_like_git_hash("1234567")); // all digits, not hash-like
    }
}
