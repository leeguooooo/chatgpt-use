//! Mode 2 · brain. ChatGPT drives local tools in an agent loop:
//!   1. seed the conversation with protocol::system_prompt(tools, task)
//!   2. channel.send → protocol::parse_reply
//!   3. if Tools: tools::execute each, protocol::render_results, send back; loop
//!   4. if Text: that's the final answer — print and stop
//! Respect --max-steps and --approve. Context accumulates inside ChatGPT, so
//! each turn only sends the new tool results, not the whole history.
//!
//! Owned by the MODES-1-2 agent.

use crate::channel::{Channel, ChannelOptions};
use crate::cli::RunArgs;
use crate::protocol::{self, Reply};
use crate::tools;
use anyhow::{anyhow, Result};
use std::path::PathBuf;

pub fn run(args: &RunArgs) -> Result<()> {
    let cwd: PathBuf = match &args.cwd {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir()?,
    };

    let tool_specs = tools::builtin_specs();

    let opts = channel_opts_from_args(args);
    let mut channel = Channel::connect(&opts)?;

    // Run the agent loop; the inner fn owns `channel` and always closes it.
    let result = agent_loop(&mut channel, &tool_specs, args, &cwd);

    channel.close();
    result
}

/// Core agent loop. Seeded with the system prompt, then drives tool calls until
/// ChatGPT returns a plain-text final answer or max_steps is exhausted.
fn agent_loop(
    channel: &mut Channel,
    tool_specs: &[crate::protocol::ToolSpec],
    args: &RunArgs,
    cwd: &PathBuf,
) -> Result<()> {
    // Seed the conversation: the system prompt teaches ChatGPT the tool protocol.
    let seed = protocol::system_prompt(tool_specs, &args.task);
    let mut reply_text = channel.send(&seed)?;

    let mut step: u32 = 0;

    loop {
        match protocol::parse_reply(&reply_text) {
            Reply::Text(answer) => {
                // ChatGPT declared the task done — print the final answer to stdout.
                println!("{answer}");
                return Ok(());
            }
            Reply::Tools(calls) => {
                step += 1;

                if step > args.max_steps {
                    eprintln!(
                        "[step {step}] max-steps limit ({}) reached without a final answer; stopping.",
                        args.max_steps
                    );
                    return Err(anyhow!(
                        "max-steps limit ({}) reached without a final answer",
                        args.max_steps
                    ));
                }

                // Print concise progress to stderr; keep stdout for the final answer only.
                for call in &calls {
                    eprintln!("[step {step}] tool: {}", call.name);
                }

                // auto_approve = !args.approve  (approve flag requests interactive confirmation)
                let auto_approve = !args.approve;
                let results: Vec<_> = calls
                    .iter()
                    .map(|call| tools::execute(call, cwd, auto_approve))
                    .collect();

                // Feed observations back to ChatGPT and get the next reply.
                let observation = protocol::render_results(&results);
                reply_text = channel
                    .send(&observation)
                    .map_err(|e| anyhow!("channel send error at step {step}: {e:#}"))?;
            }
        }
    }
}

/// Map the flattened ChannelArgs onto the engine's ChannelOptions.
fn channel_opts_from_args(args: &RunArgs) -> ChannelOptions {
    ChannelOptions {
        profile: args.channel.profile.clone(),
        session: args.channel.session.clone(),
        project: args.channel.project.clone(),
        timeout_secs: args.channel.timeout,
        model: args.channel.model.clone(),
    }
}
