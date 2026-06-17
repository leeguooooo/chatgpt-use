//! Mode 1 · sidekick. One-shot: gather optional file context, send the prompt
//! through a ChatGPT web channel, print the reply. No tool loop.
//!
//! Owned by the MODES-1-2 agent.

use crate::channel::{Channel, ChannelOptions};
use crate::cli::AskArgs;
use anyhow::{Context, Result};
use std::fs;

pub fn run(args: &AskArgs) -> Result<()> {
    let opts = channel_opts_from_args(args);

    // Build the message: prepend each --file as a fenced context block, then the prompt.
    let mut message = String::new();

    for file_path in &args.files {
        let contents = fs::read_to_string(file_path)
            .with_context(|| format!("failed to read context file: {file_path}"))?;
        message.push_str(&format!(
            "Context file: {file_path}\n```\n{contents}\n```\n\n"
        ));
    }

    message.push_str(&args.prompt);

    // Connect, send one turn, print the reply, always close.
    let mut channel = Channel::connect(&opts)?;
    let reply = channel.send(&message);
    channel.close();

    let text = reply?;
    println!("{text}");

    Ok(())
}

/// Map the flattened ChannelArgs onto the engine's ChannelOptions.
fn channel_opts_from_args(args: &AskArgs) -> ChannelOptions {
    ChannelOptions {
        profile: args.channel.profile.clone(),
        session: args.channel.session.clone(),
        project: args.channel.project.clone(),
        timeout_secs: args.channel.timeout,
        model: args.channel.model.clone(),
    }
}
