//! Mode 1 · sidekick. One-shot: gather optional file context, send the prompt
//! through a ChatGPT web channel, print the reply. No tool loop.
//!
//! For Mode::Ask the behavior is unchanged: prepend --file contents as fenced
//! blocks, send the prompt, print the raw reply.
//!
//! For non-Ask modes (Plan / Review / Debug / Research) the file contents are
//! collected into `context`, passed to `delegation::build_prompt` to produce a
//! typed DELEGATION PACKET prompt, sent to ChatGPT, and the reply is parsed by
//! `delegation::parse_packet` into a `DelegationPacket`. The packet is printed
//! either as pretty JSON (--json) or as a human-readable summary.
//!
//! Owned by the MODES-1-2 agent.

use crate::channel::{Channel, ChannelOptions};
use crate::cli::AskArgs;
use crate::delegation::{self, Mode};
use anyhow::{Context, Result};
use std::fs;

pub fn run(args: &AskArgs) -> Result<()> {
    let opts = channel_opts_from_args(args);

    if args.mode == Mode::Ask {
        run_ask_mode(args, opts)
    } else {
        run_delegation_mode(args, opts)
    }
}

// ---- Mode::Ask (unchanged plain-text behavior) ------------------------------

fn run_ask_mode(args: &AskArgs, opts: ChannelOptions) -> Result<()> {
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

// ---- Non-Ask delegation modes -----------------------------------------------

fn run_delegation_mode(args: &AskArgs, opts: ChannelOptions) -> Result<()> {
    // Collect all --file contents into one compact context block.
    let mut context = String::new();
    for file_path in &args.files {
        let contents = fs::read_to_string(file_path)
            .with_context(|| format!("failed to read context file: {file_path}"))?;
        context.push_str(&format!(
            "### File: {file_path}\n```\n{contents}\n```\n\n"
        ));
    }

    // Build the mode-typed delegation-packet prompt.
    let message = delegation::build_prompt(args.mode, &args.prompt, &context);

    // Connect, send, always close even on error.
    let mut channel = Channel::connect(&opts)?;
    let reply_result = channel.send(&message);
    channel.close();

    let reply = reply_result?;

    // Parse the structured reply into a DelegationPacket.
    let packet = delegation::parse_packet(&reply)
        .with_context(|| "ChatGPT reply did not contain a valid delegation packet")?;

    if args.json {
        // Machine-readable: emit the packet as pretty JSON.
        let json = serde_json::to_string_pretty(&packet)
            .context("failed to serialize delegation packet")?;
        println!("{json}");
    } else {
        // Human-readable summary.
        print_packet_summary(&packet);
    }

    Ok(())
}

/// Print a structured, human-readable summary of the delegation packet.
fn print_packet_summary(packet: &delegation::DelegationPacket) {
    println!("Goal: {}", packet.goal);
    println!("Verdict: {:?}", packet.verdict);

    if !packet.summary.is_empty() {
        println!("\nSummary:");
        for item in &packet.summary {
            println!("  - {item}");
        }
    }

    if !packet.plan.is_empty() {
        println!("\nPlan:");
        for step in &packet.plan {
            println!("  {}. [{}] {}", step.step, step.target, step.action);
            println!("     Success: {}", step.success_criteria);
        }
    }

    if !packet.risks.is_empty() {
        println!("\nRisks:");
        for risk in &packet.risks {
            println!("  - {risk}");
        }
    }

    if !packet.tests.is_empty() {
        println!("\nTests:");
        for test in &packet.tests {
            println!("  - {test}");
        }
    }

    if !packet.acceptance.is_empty() {
        println!("\nAcceptance:");
        for item in &packet.acceptance {
            println!("  - {item}");
        }
    }

    if !packet.do_not_do.is_empty() {
        println!("\nDo NOT do:");
        for item in &packet.do_not_do {
            println!("  - {item}");
        }
    }
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
