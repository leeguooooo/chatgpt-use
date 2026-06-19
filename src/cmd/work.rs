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
//! Prereqs: the `chatgpt-use` connector is connected in ChatGPT, and a
//! `chatgpt-use mcp --profile full` server (+ tunnel) is running so ChatGPT can
//! build/run. The connector only works on a NON-Pro model, so we default to the
//! Instant level.

use crate::channel::{Channel, ChannelOptions};
use crate::cli::WorkArgs;
use anyhow::Result;

pub fn run(args: &WorkArgs) -> Result<()> {
    // The connector requires a non-Pro model; default to Instant unless the
    // caller explicitly picked a level.
    let model = args
        .channel
        .model
        .clone()
        .or_else(|| Some("instant".to_string()));

    // Building/testing can take minutes; give it room.
    let timeout_secs = args.channel.timeout.max(600);

    let opts = ChannelOptions {
        profile: args.channel.profile.clone(),
        session: args.channel.session.clone(),
        project: args.channel.project.clone(),
        timeout_secs,
        model,
    };

    // Imperative framing: a soft prompt makes the model hedge ("I'll first
    // connect the tools…") instead of calling them. Tell it to act NOW and not
    // ask for clarification — verified to trigger real connector tool calls.
    let prompt = format!(
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

    let mut channel = Channel::connect(&opts)?;
    let reply = channel.send(&prompt);
    channel.close();

    let text = reply?;
    crate::ledger::record(
        "work",
        serde_json::json!({ "task_chars": args.task.len(), "reply_chars": text.len() }),
    );
    println!("{text}");
    Ok(())
}
