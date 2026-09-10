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
use crate::structured;
use crate::channel::{ChannelError, ErrorKind, Submitted};
use crate::receipt;
use std::path::PathBuf;
use anyhow::{Context, Result};
use std::fs;

pub fn run(args: &AskArgs) -> Result<()> {
    let opts = channel_opts_from_args(args);

    if let Some(schema) = &args.output_schema {
        if args.mode != Mode::Ask {
            anyhow::bail!("--output-schema works with plain ask only, not --mode {:?}", args.mode);
        }
        run_schema_mode(args, opts, schema)
    } else if args.mode == Mode::Ask {
        run_ask_mode(args, opts)
    } else {
        run_delegation_mode(args, opts)
    }
}

// ---- Mode::Ask (unchanged plain-text behavior) ------------------------------

/// The ask message: each --file as a fenced context block, then the prompt.
fn ask_message(args: &AskArgs) -> Result<String> {
    let mut message = String::new();

    for file_path in &args.files {
        let contents = fs::read_to_string(file_path)
            .with_context(|| format!("failed to read context file: {file_path}"))?;
        message.push_str(&format!(
            "Context file: {file_path}\n```\n{contents}\n```\n\n"
        ));
    }

    message.push_str(&args.prompt);
    Ok(message)
}

fn run_ask_mode(args: &AskArgs, mut opts: ChannelOptions) -> Result<()> {
    let message = ask_message(args)?;
    opts.receipt = claim_receipt(args)?;

    // Connect, send one turn, print the reply, always close.
    let reply = Channel::connect(&opts).and_then(|mut channel| {
        let reply = channel.send(&message);
        channel.close();
        reply
    });
    let envelope = match &reply {
        Ok(_) => serde_json::json!({"status": "completed"}),
        Err(e) => structured::failure(e),
    };
    finish_receipt(opts.receipt.as_deref(), &envelope);

    let text = reply?;
    crate::ledger::record(
        "ask",
        serde_json::json!({
            "prompt_chars": args.prompt.len(),
            "files": args.files.len(),
            "model": args.channel.model,
            "reply_chars": text.len(),
        }),
    );
    println!("{text}");

    Ok(())
}

// ---- --output-schema --------------------------------------------------------

/// Structured ask. Every outcome, failures included, is ONE JSON envelope on
/// stdout with an exit code to match (see `structured`); progress stays on
/// stderr. Never returns Err: a caller parsing stdout must always get a line.
fn run_schema_mode(args: &AskArgs, mut opts: ChannelOptions, schema_path: &str) -> Result<()> {
    let claimed = match structured::Schema::load(schema_path) {
        Err(why) => Err(structured::schema_error(&why)),
        Ok(schema) => match ask_message(args) {
            Err(e) => Err(structured::failure(&e)),
            Ok(request) => match claim_receipt(args) {
                Err(e) => Err(structured::failure(&e)),
                Ok(path) => {
                    opts.receipt = path;
                    Ok((schema, request))
                }
            },
        },
    };
    let mut envelope = match claimed {
        Err(envelope) => envelope,
        Ok((schema, request)) => {
            {
                let message = structured::build_message(&request, &schema);
                match Channel::connect(&opts) {
                    Err(e) => structured::failure(&e),
                    Ok(mut channel) => {
                        let reply = channel.send(&message);
                        let convo = channel.conversation_id().map(str::to_string);
                        channel.close();
                        let mut envelope = structured::evaluate(reply, &schema);
                        if let Some(id) = convo {
                            envelope["conversation_id"] = id.into();
                        }
                        envelope
                    }
                }
            }
        }
    };
    finish_receipt(opts.receipt.as_deref(), &envelope);
    if let Some(id) = &args.request_id {
        envelope["request_id"] = id.as_str().into();
    }

    let status = envelope["status"].as_str().unwrap_or("failed").to_string();
    crate::ledger::record(
        "ask_schema",
        serde_json::json!({
            "prompt_chars": args.prompt.len(),
            "files": args.files.len(),
            "model": args.channel.model,
            "status": status,
        }),
    );
    println!("{envelope}");
    let code = structured::exit_code(&status);
    if code != 0 {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }
    Ok(())
}

/// Claim the receipt for `--request-id` before anything is sent. Refuses an
/// id whose earlier request may have reached ChatGPT: the caller asked for a
/// receipt precisely so that a lost reply is looked up, not sent twice.
fn claim_receipt(args: &AskArgs) -> Result<Option<PathBuf>> {
    let Some(id) = &args.request_id else { return Ok(None) };
    if !receipt::valid_id(id) {
        anyhow::bail!("invalid --request-id {id:?}: use letters, digits, '.', '_' or '-' (up to 128)");
    }
    let path = receipt::path_for(id);
    let fresh = receipt::Receipt::accepted(id);
    if receipt::create(&path, &fresh)
        .with_context(|| format!("could not write receipt {}", path.display()))?
    {
        return Ok(Some(path));
    }
    // The id is taken. Reuse it only if its receipt proves nothing was sent;
    // an unreadable receipt proves nothing, so it counts as taken.
    let existing = receipt::load(&path);
    if !receipt::may_send(existing.as_ref()) || existing.is_none() {
        let (state, submitted) = existing
            .as_ref()
            .map(|r| (r.state.as_str(), r.submitted.as_str()))
            .unwrap_or(("unreadable", "unknown"));
        // `submitted` describes the EARLIER request — the one a caller must
        // not resend — not this refused call, which sent nothing.
        let prior = if submitted == "yes" { Submitted::Yes } else { Submitted::Unknown };
        return Err(ChannelError::new(
            ErrorKind::Duplicate,
            format!(
                "request {id:?} already exists (state {state}, submitted {submitted}); not \
                 sending it again. Look it up with: chatgpt-use status {id}"
            ),
        )
        .with_submitted(prior)
        .into());
    }
    receipt::save(&path, &fresh)
        .with_context(|| format!("could not write receipt {}", path.display()))?;
    Ok(Some(path))
}

/// Close the receipt with the turn's outcome. A reply that exists (even one
/// that failed validation) means the turn completed and the prompt was sent;
/// otherwise the failure says how far it got.
fn finish_receipt(path: Option<&std::path::Path>, envelope: &serde_json::Value) {
    let Some(path) = path else { return };
    let status = envelope["status"].as_str().unwrap_or("failed").to_string();
    let replied = matches!(status.as_str(), "completed" | "schema_violation" | "unparseable");
    let submitted = if replied {
        "yes".to_string()
    } else {
        envelope["error"]["submitted"].as_str().unwrap_or("unknown").to_string()
    };
    let error = envelope["error"]["message"].as_str().map(str::to_string);
    let convo = envelope["conversation_id"].as_str().map(str::to_string);
    receipt::update(path, |r| {
        r.state = if replied { "completed" } else { "failed" }.into();
        // Never downgrade: the channel may already have recorded the prompt
        // as sent, which a later "not submitted" cannot undo.
        if !(r.submitted == "yes" && submitted != "yes") {
            r.submitted = submitted;
        }
        r.outcome = Some(status);
        r.error = error;
        if convo.is_some() {
            r.conversation_id = convo;
        }
    });
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

    crate::ledger::record(
        "delegate",
        serde_json::json!({
            "mode": format!("{:?}", args.mode),
            "goal": packet.goal,
            "verdict": format!("{:?}", packet.verdict),
            "model": args.channel.model,
        }),
    );

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
        busy_fail: args.channel.busy == crate::cli::BusyPolicy::Fail,
        receipt: None,
    }
}
