//! Mode 1 · sidekick. One-shot: gather optional file and stdin context, send
//! the prompt through a ChatGPT web channel, report the reply. No tool loop.
//!
//! Plain ask is split in two so every front end reports the same thing:
//!
//! - [`execute`] is the core. It sends one turn and returns ONE envelope
//!   (see `structured`), failures included. It never prints to stdout, never
//!   exits and never reads stdin, so the caller-facing MCP server can call it
//!   without a schema error killing the server or a stray line corrupting
//!   JSON-RPC.
//! - [`run`] is the CLI adapter: it reads stdin, calls the core, then prints
//!   the reply text (the default), or the envelope (`--json`,
//!   `--output-schema`) with the matching exit code.
//!
//! For non-Ask modes (Plan / Review / Debug / Research) the context is passed
//! to `delegation::build_prompt` to produce a typed DELEGATION PACKET prompt,
//! and the reply is parsed by `delegation::parse_packet`. The packet is printed
//! either as pretty JSON (--json) or as a human-readable summary.

use crate::channel::{Channel, ChannelOptions};
use crate::cli::AskArgs;
use crate::delegation::{self, Mode};
use crate::structured;
use crate::channel::{ChannelError, ErrorKind, Submitted};
use crate::receipt;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;

/// Largest stdin accepted as context. Beyond this a prompt is unlikely to fit
/// ChatGPT's message limit anyway, and failing before sending is cheaper than
/// a rejected turn.
pub const STDIN_LIMIT: usize = 512 * 1024;
/// How long default (auto) mode waits for the first byte from a piped stdin.
/// Some harnesses hand a child an open pipe they never write to or close; a
/// plain read would hang there for good.
const STDIN_FIRST_BYTE_WAIT: Duration = Duration::from_secs(5);
/// How long default mode waits, in all, for a piped stdin to close.
const STDIN_TOTAL_WAIT: Duration = Duration::from_secs(60);

/// One ask, as every front end describes it.
pub struct AskInput {
    pub prompt: String,
    /// Context file paths, read by [`execute`].
    pub files: Vec<String>,
    /// Context text already in hand (the CLI's stdin, an MCP argument).
    pub stdin: Option<String>,
    /// Ask for JSON matching this schema and validate it.
    pub schema: Option<SchemaSource>,
    pub request_id: Option<String>,
}

pub enum SchemaSource {
    Path(String),
}

pub fn run(args: &AskArgs) -> Result<()> {
    let opts = channel_opts_from_args(args);
    // A request with a receipt is one a caller may cancel: SIGTERM stops its
    // reply and records how that ended, instead of killing us mid-generation.
    if args.request_id.is_some() {
        crate::channel::install_cancel_handler();
    }
    if args.output_schema.is_some() && args.mode != Mode::Ask {
        anyhow::bail!("--output-schema works with plain ask only, not --mode {:?}", args.mode);
    }

    let mode = if args.no_stdin {
        StdinMode::Skip
    } else if args.stdin {
        StdinMode::Wait
    } else {
        StdinMode::Auto
    };
    let stdin = read_stdin(mode);

    if args.mode != Mode::Ask {
        let stdin = match stdin {
            StdinRead::Text(t) => Some(t),
            StdinRead::None => None,
            StdinRead::Failed(e) => return Err(e),
        };
        return run_delegation_mode(args, opts, stdin.as_deref());
    }

    let machine = args.json || args.output_schema.is_some();
    let mut envelope = match stdin {
        StdinRead::Failed(e) => {
            // Nothing was claimed or sent; still one envelope, still the id.
            let mut env = structured::failure(&e);
            if let Some(id) = &args.request_id {
                env["request_id"] = id.as_str().into();
            }
            env
        }
        read => {
            let input = AskInput {
                prompt: args.prompt.clone(),
                files: args.files.clone(),
                stdin: match read {
                    StdinRead::Text(t) => Some(t),
                    _ => None,
                },
                schema: args.output_schema.clone().map(SchemaSource::Path),
                request_id: args.request_id.clone(),
            };
            execute(&input, opts)
        }
    };

    let status = envelope["status"].as_str().unwrap_or("failed").to_string();
    if machine {
        println!("{envelope}");
        let code = structured::exit_code(&status);
        if code != 0 {
            use std::io::Write;
            let _ = std::io::stdout().flush();
            std::process::exit(code);
        }
        return Ok(());
    }
    // Plain text, as before: the reply on stdout, a failure as an error.
    if status == "completed" {
        if let Some(text) = envelope["result"]["text"].as_str() {
            println!("{text}");
            return Ok(());
        }
    }
    let message = envelope["error"]["message"]
        .take()
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("ask ended with status {status}"));
    Err(anyhow::anyhow!(message))
}

/// The ask core: one turn, one envelope. See the module docs for what it
/// must never do.
pub fn execute(input: &AskInput, mut opts: ChannelOptions) -> Value {
    let schema = match &input.schema {
        None => None,
        Some(source) => match load_schema(source) {
            Ok(s) => Some(s),
            Err(why) => return tag(structured::schema_error(&why), input),
        },
    };
    let request = match ask_message(input) {
        Ok(m) => m,
        Err(e) => return tag(structured::failure(&e), input),
    };
    match claim_receipt(input.request_id.as_deref()) {
        Ok(path) => opts.receipt = path,
        Err(e) => return tag(structured::failure(&e), input),
    }
    let message = match &schema {
        Some(s) => structured::build_message(&request, s),
        None => request,
    };

    let mut convo = None;
    let reply = Channel::connect(&opts).and_then(|mut channel| {
        let reply = channel.send(&message);
        convo = channel.conversation_id().map(str::to_string);
        channel.close();
        reply
    });
    let mut envelope = match (&schema, reply) {
        (Some(s), reply) => structured::evaluate(reply, s),
        (None, Ok(text)) => serde_json::json!({"status": "completed", "result": {"text": text}}),
        (None, Err(e)) => structured::failure(&e),
    };
    if let Some(id) = convo {
        envelope["conversation_id"] = id.into();
    }
    finish_receipt(opts.receipt.as_deref(), &envelope);

    crate::ledger::record(
        if schema.is_some() { "ask_schema" } else { "ask" },
        serde_json::json!({
            "prompt_chars": input.prompt.len(),
            "files": input.files.len(),
            "stdin_chars": input.stdin.as_ref().map(|s| s.len()),
            "model": opts.model,
            "status": envelope["status"],
        }),
    );
    tag(envelope, input)
}

/// Stamp the caller's request id on an envelope.
fn tag(mut envelope: Value, input: &AskInput) -> Value {
    if let Some(id) = &input.request_id {
        envelope["request_id"] = id.as_str().into();
    }
    envelope
}

fn load_schema(source: &SchemaSource) -> Result<structured::Schema, String> {
    match source {
        SchemaSource::Path(p) => structured::Schema::load(p),
    }
}

// ---- context ----------------------------------------------------------------

/// The ask message: each context file, then stdin, as fenced blocks, then the
/// prompt.
fn ask_message(input: &AskInput) -> Result<String> {
    let mut files = Vec::new();
    for path in &input.files {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read context file: {path}"))?;
        files.push((path.clone(), contents));
    }
    Ok(compose(&files, input.stdin.as_deref(), &input.prompt))
}

/// Pure assembly of the ask message.
fn compose(files: &[(String, String)], stdin: Option<&str>, prompt: &str) -> String {
    let mut message = String::new();
    for (path, contents) in files {
        message.push_str(&format!("Context file: {path}\n{}\n\n", fenced(contents)));
    }
    if let Some(text) = stdin.filter(|t| !t.trim().is_empty()) {
        message.push_str(&format!("Context from stdin:\n{}\n\n", fenced(text)));
    }
    message.push_str(prompt);
    message
}

/// `text` in a code fence longer than any backtick run inside it, so content
/// that itself holds ``` (a diff of a Markdown file, say) cannot end it early.
fn fenced(text: &str) -> String {
    let longest = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}\n{}\n{fence}", text.trim_end_matches('\n'))
}

// ---- stdin ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdinMode {
    /// Read it if it is not a terminal, within [`AUTO_WAIT`].
    Auto,
    /// Read it to EOF, however long that takes.
    Wait,
    Skip,
}

enum StdinRead {
    Text(String),
    None,
    Failed(anyhow::Error),
}

/// How long `ask` waits on stdin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StdinWait {
    /// For the first byte (or EOF). `None` waits for good.
    first: Option<Duration>,
    /// For EOF, counted from the start. `None` waits for good.
    total: Option<Duration>,
}

/// Default mode: an explicit deadline at each stage, so neither a pipe that
/// never speaks nor one that never closes can hang the run.
const AUTO_WAIT: StdinWait = StdinWait {
    first: Some(STDIN_FIRST_BYTE_WAIT),
    total: Some(STDIN_TOTAL_WAIT),
};
/// `--stdin`: the caller said the context is coming, so wait for it.
const EXPLICIT_WAIT: StdinWait = StdinWait { first: None, total: None };

fn read_stdin(mode: StdinMode) -> StdinRead {
    let wait = match mode {
        StdinMode::Skip => return StdinRead::None,
        StdinMode::Auto if std::io::stdin().is_terminal() => return StdinRead::None,
        StdinMode::Auto => AUTO_WAIT,
        StdinMode::Wait => EXPLICIT_WAIT,
    };
    read_capped(std::io::stdin(), STDIN_LIMIT, wait)
}

fn not_submitted(message: String) -> StdinRead {
    StdinRead::Failed(ChannelError::new(ErrorKind::NotSubmitted, message).into())
}

/// Read `r` to EOF on a helper thread, up to `cap` bytes, within `wait`.
///
/// A deadline that passes FAILS the ask (`not_submitted`) rather than sending
/// it without the context: a slow `git diff` is still a diff the caller meant
/// to send, and answering without it would look like success. An abandoned
/// reader thread stays parked on its read and ends with the process.
fn read_capped<R: Read + Send + 'static>(mut r: R, cap: usize, wait: StdinWait) -> StdinRead {
    enum Ev {
        Started,
        Done(std::io::Result<Vec<u8>>),
        TooLarge,
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut started = false;
        loop {
            match r.read(&mut chunk) {
                Ok(n) => {
                    if !started {
                        started = true;
                        let _ = tx.send(Ev::Started);
                    }
                    if n == 0 {
                        let _ = tx.send(Ev::Done(Ok(buf)));
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > cap {
                        let _ = tx.send(Ev::TooLarge);
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = tx.send(Ev::Done(Err(e)));
                    return;
                }
            }
        }
    });

    let begun = Instant::now();
    let mut started = false;
    loop {
        // The next deadline: the first byte until one arrives, then EOF.
        let limit = if started { wait.total } else { wait.first.or(wait.total) };
        let ev = match limit {
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            Some(d) => rx.recv_timeout(d.saturating_sub(begun.elapsed())),
        };
        match ev {
            Ok(Ev::Started) => started = true,
            Ok(Ev::Done(Ok(bytes))) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                return if text.trim().is_empty() { StdinRead::None } else { StdinRead::Text(text) };
            }
            Ok(Ev::Done(Err(e))) => return not_submitted(format!("could not read stdin: {e}")),
            Ok(Ev::TooLarge) => {
                return not_submitted(format!(
                    "stdin is larger than {} KiB; nothing was sent. Trim it, or pass the \
                     relevant part with --file",
                    cap / 1024
                ))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let secs = begun.elapsed().as_secs();
                return not_submitted(if started {
                    format!(
                        "stdin was still open after {secs}s; nothing was sent. Pass --stdin to \
                         wait for it to close, or --no-stdin to ignore it"
                    )
                } else {
                    format!(
                        "stdin is open but sent nothing in {secs}s; nothing was sent. Pass \
                         --no-stdin if there is no context to pipe in, or --stdin to wait for it"
                    )
                });
            }
            // The reader always reports before it ends.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return not_submitted("stdin reader stopped unexpectedly".into())
            }
        }
    }
}

/// Claim the receipt for `--request-id` before anything is sent. Refuses an
/// id whose earlier request may have reached ChatGPT: the caller asked for a
/// receipt precisely so that a lost reply is looked up, not sent twice.
fn claim_receipt(request_id: Option<&str>) -> Result<Option<PathBuf>> {
    let Some(id) = request_id else { return Ok(None) };
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

/// Close the receipt, if there is one, with the turn's outcome.
fn finish_receipt(path: Option<&std::path::Path>, envelope: &serde_json::Value) {
    if let Some(path) = path {
        receipt::finish(path, envelope);
    }
}

// ---- Non-Ask delegation modes -----------------------------------------------

fn run_delegation_mode(args: &AskArgs, opts: ChannelOptions, stdin: Option<&str>) -> Result<()> {
    // Collect all --file contents (and stdin) into one compact context block.
    let mut context = String::new();
    for file_path in &args.files {
        let contents = fs::read_to_string(file_path)
            .with_context(|| format!("failed to read context file: {file_path}"))?;
        context.push_str(&format!("### File: {file_path}\n{}\n\n", fenced(&contents)));
    }
    if let Some(text) = stdin {
        context.push_str(&format!("### stdin\n{}\n\n", fenced(text)));
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
        ignore_cooldown: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(prompt: &str) -> AskInput {
        AskInput {
            prompt: prompt.into(),
            files: vec![],
            stdin: None,
            schema: None,
            request_id: None,
        }
    }

    fn opts() -> ChannelOptions {
        ChannelOptions {
            profile: "auto".into(),
            session: None,
            project: String::new(),
            timeout_secs: 5,
            model: None,
            busy_fail: true,
            receipt: None,
            ignore_cooldown: false,
        }
    }

    #[test]
    fn compose_puts_files_then_stdin_then_prompt() {
        let m = compose(&[("a.rs".into(), "fn a() {}\n".into())], Some("diff --git x"), "review");
        let (a, s, p) = (m.find("a.rs").unwrap(), m.find("diff --git").unwrap(), m.rfind("review").unwrap());
        assert!(a < s && s < p, "{m}");
        assert!(m.contains("Context from stdin:"), "{m}");
        assert!(m.ends_with("review"));
    }

    #[test]
    fn blank_stdin_adds_nothing() {
        assert_eq!(compose(&[], Some("  \n"), "q"), "q");
        assert_eq!(compose(&[], None, "q"), "q");
    }

    #[test]
    fn fences_outgrow_backticks_in_the_content() {
        let f = fenced("x\n```\ny\n````\nz");
        assert!(f.starts_with("`````\n") && f.ends_with("\n`````"), "{f}");
        assert!(fenced("plain").starts_with("```\n"));
    }

    const QUICK: StdinWait = StdinWait {
        first: Some(Duration::from_millis(300)),
        total: Some(Duration::from_millis(900)),
    };

    fn failure_message(read: StdinRead) -> String {
        match read {
            StdinRead::Failed(e) => {
                let env = structured::failure(&e);
                assert_eq!(env["status"], "failed");
                assert_eq!(env["error"]["kind"], "not_submitted");
                assert_eq!(env["error"]["submitted"], "no");
                env["error"]["message"].as_str().unwrap().to_string()
            }
            StdinRead::Text(t) => panic!("expected a failure, got text {t:?}"),
            StdinRead::None => panic!("expected a failure, got no context"),
        }
    }

    /// A scripted stdin: each step waits, then yields bytes, EOF or an error;
    /// after the script it blocks for good, like a pipe nobody closes.
    enum Step {
        Wait(u64),
        Bytes(&'static [u8]),
        Eof,
        Fail,
    }
    struct Script(std::collections::VecDeque<Step>);
    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            loop {
                match self.0.pop_front() {
                    Some(Step::Wait(ms)) => std::thread::sleep(Duration::from_millis(ms)),
                    Some(Step::Bytes(b)) => {
                        buf[..b.len()].copy_from_slice(b);
                        return Ok(b.len());
                    }
                    Some(Step::Eof) => return Ok(0),
                    Some(Step::Fail) => return Err(std::io::Error::other("broken pipe")),
                    None => std::thread::sleep(Duration::from_secs(3600)),
                }
            }
        }
    }
    fn script(steps: Vec<Step>) -> Script {
        Script(steps.into())
    }

    #[test]
    fn reads_piped_text_to_eof() {
        let r = std::io::Cursor::new(b"hello\nworld\n".to_vec());
        match read_capped(r, 1024, QUICK) {
            StdinRead::Text(t) => assert_eq!(t, "hello\nworld\n"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn empty_stdin_is_no_context() {
        let r = std::io::Cursor::new(Vec::new());
        assert!(matches!(read_capped(r, 1024, QUICK), StdinRead::None));
    }

    #[test]
    fn oversized_stdin_fails_before_sending() {
        let r = std::io::Cursor::new(vec![b'x'; 5000]);
        assert!(failure_message(read_capped(r, 1024, EXPLICIT_WAIT)).contains("larger than"));
    }

    #[test]
    fn a_read_error_on_the_first_read_is_a_failure_not_a_panic() {
        for wait in [QUICK, EXPLICIT_WAIT] {
            let msg = failure_message(read_capped(script(vec![Step::Fail]), 1024, wait));
            assert!(msg.contains("could not read stdin"), "{msg}");
        }
    }

    #[test]
    fn a_read_error_mid_stream_is_a_failure() {
        let r = script(vec![Step::Bytes(b"part"), Step::Fail]);
        assert!(failure_message(read_capped(r, 1024, QUICK)).contains("could not read stdin"));
    }

    #[test]
    fn a_silent_open_pipe_fails_closed_in_auto_mode() {
        let started = Instant::now();
        let msg = failure_message(read_capped(script(vec![]), 1024, QUICK));
        assert!(msg.contains("sent nothing") && msg.contains("--no-stdin"), "{msg}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_late_first_byte_fails_rather_than_dropping_the_context() {
        let r = script(vec![Step::Wait(600), Step::Bytes(b"slow diff"), Step::Eof]);
        assert!(failure_message(read_capped(r, 1024, QUICK)).contains("sent nothing"));
    }

    #[test]
    fn a_first_byte_in_time_then_a_prompt_close_is_read() {
        let r = script(vec![Step::Wait(100), Step::Bytes(b"diff"), Step::Wait(100), Step::Eof]);
        assert!(matches!(read_capped(r, 1024, QUICK), StdinRead::Text(t) if t == "diff"));
    }

    #[test]
    fn a_pipe_that_never_closes_hits_the_total_deadline() {
        let started = Instant::now();
        let r = script(vec![Step::Bytes(b"some")]);
        let msg = failure_message(read_capped(r, 1024, QUICK));
        assert!(msg.contains("still open") && msg.contains("--stdin"), "{msg}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn explicit_stdin_waits_past_the_auto_deadlines() {
        let r = script(vec![Step::Wait(1200), Step::Bytes(b"late"), Step::Wait(200), Step::Eof]);
        assert!(matches!(read_capped(r, 1024, EXPLICIT_WAIT), StdinRead::Text(t) if t == "late"));
    }

    #[test]
    fn execute_reports_an_unreadable_file_as_one_envelope() {
        let mut i = input("q");
        i.files = vec!["/nonexistent/cgu-ask-test".into()];
        i.request_id = Some("cgu-test-unreadable".into());
        let env = execute(&i, opts());
        assert_eq!(env["status"], "failed");
        assert_eq!(env["error"]["submitted"], "no");
        assert_eq!(env["request_id"], "cgu-test-unreadable");
        assert!(env["error"]["message"].as_str().unwrap().contains("cgu-ask-test"));
        // Failed before the receipt was claimed: the id stays free.
        assert!(receipt::load(&receipt::path_for("cgu-test-unreadable")).is_none());
    }

    #[test]
    fn execute_reports_a_bad_schema_as_schema_error() {
        let mut i = input("q");
        i.schema = Some(SchemaSource::Path("/nonexistent/schema.json".into()));
        let env = execute(&i, opts());
        assert_eq!(env["status"], "schema_error");
        assert_eq!(structured::exit_code("schema_error"), 8);
    }
}
