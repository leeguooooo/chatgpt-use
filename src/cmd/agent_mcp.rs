//! `agent-mcp` — a caller-facing MCP server on stdio. The agent the user works
//! in (Codex, Claude Code, any MCP client) launches it and gets four tools:
//! `ask`, `status`, `resume`, `cancel`. Each returns the same envelope as the
//! CLI's `--json` output, so every harness sees one contract.
//!
//! This is not `mcp`: that server is called BY ChatGPT through a tunnel and
//! exposes local file and shell tools. This one is called by your agent and
//! exposes nothing but the ChatGPT channel.
//!
//! What it guarantees:
//!
//! - **The read loop never blocks on a turn.** `ask` and `resume` run on
//!   worker threads; `status`, `cancel` and protocol messages are answered
//!   while one runs. Responses can therefore arrive out of order, as JSON-RPC
//!   allows.
//! - **One browser turn at a time.** A second `ask`/`resume` while one runs is
//!   answered at once with status `busy`; it is never queued blind.
//! - **Cancelling one request never touches another, or the server.** Each
//!   request has its own cancel flag, scoped to its worker thread (see
//!   `channel::with_cancel_flag`). Its receipt names the server as owner, so
//!   `chatgpt-use cancel <id>` from a shell writes a cancel marker this server
//!   watches instead of signalling the server's pid.
//! - **Only JSON-RPC on stdout.** The ask core never prints to stdout; all
//!   progress goes to stderr.

use crate::channel::{with_cancel_flag, ChannelOptions};
use crate::cli::{AgentMcpArgs, CancelArgs, ChannelArgs, ResumeArgs};
use crate::cmd::ask::{self, AskInput, SchemaSource};
use crate::receipt;
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Newest first; an unknown requested version is answered with the newest.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// How long a `cancel` call waits for the request it stopped to finish.
const CANCEL_WAIT: Duration = Duration::from_secs(60);
/// How often a running ask reports progress, when the client asked for it.
/// Several clients (Pi among them) reset their per-call timeout on each
/// progress notification, so a turn of many minutes outlives a 60s default.
const PROGRESS_EVERY: Duration = Duration::from_secs(15);

/// The ChatGPT side of each tool. The server owns protocol, concurrency and
/// cancellation; a backend only runs one operation and returns its envelope.
pub trait Backend: Send + Sync + 'static {
    /// One ask. `cancel` is this request's own flag.
    fn ask(&self, input: AskInput, opts: ChannelOptions, cancel: Arc<AtomicBool>) -> Value;
    fn status(&self, request_id: &str) -> Value;
    fn resume(
        &self,
        request_id: &str,
        schema: Option<Value>,
        channel: ChannelArgs,
        owner_token: &str,
        cancel: Arc<AtomicBool>,
    ) -> Value;
    /// Cancel a request this server does not hold.
    fn cancel(&self, request_id: &str, channel: ChannelArgs) -> Value;
}

/// The real backend: the same cores the CLI commands use.
struct Live;

impl Backend for Live {
    fn ask(&self, input: AskInput, opts: ChannelOptions, cancel: Arc<AtomicBool>) -> Value {
        with_cancel_flag(cancel, || ask::execute(&input, opts))
    }
    fn status(&self, request_id: &str) -> Value {
        crate::cmd::status::envelope(request_id)
    }
    fn resume(
        &self,
        request_id: &str,
        schema: Option<Value>,
        channel: ChannelArgs,
        owner_token: &str,
        cancel: Arc<AtomicBool>,
    ) -> Value {
        let args = ResumeArgs { request_id: request_id.into(), output_schema: None, channel };
        with_cancel_flag(cancel, || {
            crate::cmd::resume::resume(&args, Some((receipt::OWNER_MCP, owner_token)), schema)
        })
    }
    fn cancel(&self, request_id: &str, channel: ChannelArgs) -> Value {
        crate::cmd::cancel::cancel(&CancelArgs { request_id: request_id.into(), channel })
    }
}

pub fn run(args: &AgentMcpArgs) -> Result<()> {
    let server = Server::new(Live, Box::new(std::io::stdout()), args.channel.clone());
    eprintln!("chatgpt-use agent-mcp: serving on stdio");
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if !line.trim().is_empty() {
            server.handle(&line);
        }
    }
    // The client hung up. Stop what is still running so its receipt records
    // how it ended, rather than leaving a generation nobody will read.
    server.shutdown(Duration::from_secs(30));
    Ok(())
}

struct Task {
    flag: Arc<AtomicBool>,
    /// The JSON-RPC id of the call running it.
    rpc_id: Value,
    /// Set by `notifications/cancelled`: the client has said it will ignore
    /// the response, so none is sent.
    silent: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    /// The request holding the one browser turn, if any.
    slot: Option<String>,
    tasks: HashMap<String, Task>,
    /// Final status of requests this server ran, for `cancel` to report.
    finished: HashMap<String, String>,
}

pub struct Server<B: Backend> {
    backend: Arc<B>,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
    state: Arc<Mutex<State>>,
    defaults: ChannelArgs,
    seq: AtomicU64,
    progress_every: Duration,
    /// Where a run's cancel marker lives (request id, owner token); tests
    /// point it elsewhere.
    marker_path: fn(&str, &str) -> std::path::PathBuf,
}

impl<B: Backend> Server<B> {
    pub fn new(backend: B, out: Box<dyn Write + Send>, defaults: ChannelArgs) -> Arc<Self> {
        Arc::new(Server {
            backend: Arc::new(backend),
            out: Arc::new(Mutex::new(out)),
            state: Arc::new(Mutex::new(State::default())),
            defaults,
            seq: AtomicU64::new(0),
            progress_every: PROGRESS_EVERY,
            marker_path: receipt::cancel_marker,
        })
    }

    fn send(&self, msg: Value) {
        let mut out = self.out.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(out, "{msg}");
        let _ = out.flush();
    }

    fn reply(&self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn error(&self, id: &Value, code: i64, message: impl Into<String>) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}}));
    }

    /// Handle one inbound line. Returns the worker thread when the call was
    /// handed to one (tests join it; the server does not need to).
    pub fn handle(self: &Arc<Self>, line: &str) -> Option<JoinHandle<()>> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                self.error(&Value::Null, -32700, format!("parse error: {e}"));
                return None;
            }
        };
        let (id, method, params) = match classify(&msg) {
            Inbound::Request { id, method, params } => (id, method, params),
            Inbound::Notification { method, params } => {
                self.notification(&method, &params);
                return None;
            }
            // A response to a request of ours: we send none, so nothing to do.
            Inbound::Response => return None,
            Inbound::Invalid { id, why } => {
                self.error(&id, -32600, format!("invalid request: {why}"));
                return None;
            }
        };
        let method = method.as_str();
        match method {
            "initialize" => {
                self.reply(&id, initialize_result(&params));
                None
            }
            "ping" => {
                self.reply(&id, json!({}));
                None
            }
            "tools/list" => {
                self.reply(&id, json!({"tools": tool_list()}));
                None
            }
            "tools/call" => self.tools_call(id, &params),
            other => {
                self.error(&id, -32601, format!("method not found: {other}"));
                None
            }
        }
    }

    fn notification(&self, method: &str, params: &Value) {
        if method == "notifications/cancelled" {
            let Some(rpc) = params.get("requestId") else { return };
            let state = self.state.lock().unwrap();
            for task in state.tasks.values().filter(|t| &t.rpc_id == rpc) {
                task.silent.store(true, Ordering::SeqCst);
                task.flag.store(true, Ordering::SeqCst);
            }
        }
        // notifications/initialized and anything else: no response, by spec.
    }

    fn tools_call(self: &Arc<Self>, id: Value, params: &Value) -> Option<JoinHandle<()>> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = match params.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => {
                self.error(&id, -32602, "arguments must be an object");
                return None;
            }
        };
        let Some(spec) = tool_list().into_iter().find(|t| t["name"] == name) else {
            self.error(&id, -32602, format!("unknown tool: {name:?}"));
            return None;
        };
        if let Err(why) = check_args(&spec["inputSchema"], &args) {
            self.error(&id, -32602, format!("invalid arguments for {name}: {why}"));
            return None;
        }
        let progress = match params.pointer("/_meta/progressToken") {
            None => None,
            Some(t) if t.is_string() || t.is_number() => Some(t.clone()),
            Some(_) => {
                self.error(&id, -32602, "_meta.progressToken must be a string or a number");
                return None;
            }
        };
        match name {
            "ask" => self.ask(id, args, progress),
            "resume" => self.resume(id, args, progress),
            "status" => {
                self.status(id, &args);
                None
            }
            "cancel" => self.cancel(id, &args),
            _ => unreachable!("tool_list and this match name the same tools"),
        }
    }

    /// Claim the browser slot for `request_id`, or say why not.
    fn claim(&self, request_id: &str, rpc_id: &Value) -> Result<(Arc<AtomicBool>, Arc<AtomicBool>), Value> {
        let mut state = self.state.lock().unwrap();
        if state.tasks.contains_key(request_id) {
            return Err(json!({
                "status": "duplicate",
                "error": {"kind": "duplicate_request", "submitted": "unknown",
                          "message": format!("request {request_id:?} is already running here")},
            }));
        }
        if let Some(holder) = &state.slot {
            return Err(json!({
                "status": "busy",
                "error": {"kind": "busy", "submitted": "no",
                          "message": format!("request {holder:?} is using the ChatGPT window; \
                                              nothing was sent. Retry when it finishes")},
            }));
        }
        let flag = Arc::new(AtomicBool::new(false));
        let silent = Arc::new(AtomicBool::new(false));
        state.slot = Some(request_id.to_string());
        state.tasks.insert(
            request_id.to_string(),
            Task { flag: flag.clone(), rpc_id: rpc_id.clone(), silent: silent.clone() },
        );
        Ok((flag, silent))
    }

    /// Run `work` for `request_id` on a worker thread holding the browser
    /// slot, watching the request's cancel marker, then reply.
    fn spawn_turn(
        self: &Arc<Self>,
        rpc_id: Value,
        progress: Option<Value>,
        request_id: String,
        owner_token: String,
        work: impl FnOnce(&B, Arc<AtomicBool>) -> Value + Send + 'static,
    ) -> Option<JoinHandle<()>> {
        let (flag, silent) = match self.claim(&request_id, &rpc_id) {
            Ok(f) => f,
            Err(mut envelope) => {
                envelope["request_id"] = request_id.into();
                self.reply(&rpc_id, tool_result(envelope, false));
                return None;
            }
        };
        let me = self.clone();
        Some(std::thread::spawn(move || {
            let done = Arc::new(AtomicBool::new(false));
            let marker = (me.marker_path)(&request_id, &owner_token);
            let watcher = watch_marker(marker.clone(), flag.clone(), done.clone());
            let ticker = progress.map(|token| me.report_progress(token, done.clone()));
            // A panic in the work must still free the slot and answer the
            // call; otherwise the server reads as busy for good.
            let backend = me.backend.clone();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&backend, flag)));
            done.store(true, Ordering::SeqCst);
            let _ = watcher.join();
            if let Some(t) = ticker {
                let _ = t.join();
            }
            // Only this run's own file: nobody else's marker shares its name.
            let _ = std::fs::remove_file(&marker);
            let mut envelope = outcome.unwrap_or_else(|panic| {
                let why = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".into());
                // Where it stopped is unknown, so whether the prompt went out is too.
                let failed = json!({"status": "failed", "error": {"kind": "internal",
                    "message": format!("internal error while handling the request: {why}"),
                    "submitted": "unknown"}});
                finish_if_ours(&request_id, &owner_token, &failed);
                failed
            });
            envelope["request_id"] = request_id.as_str().into();
            let status = envelope["status"].as_str().unwrap_or("failed").to_string();
            {
                let mut state = me.state.lock().unwrap();
                state.tasks.remove(&request_id);
                state.slot = None;
                if state.finished.len() > 1000 {
                    state.finished.clear();
                }
                state.finished.insert(request_id, status.clone());
            }
            if !silent.load(Ordering::SeqCst) {
                me.reply(&rpc_id, tool_result(envelope, status == "completed"));
            }
        }))
    }

    /// Send `notifications/progress` for `token` every `progress_every`
    /// until `done`.
    fn report_progress(self: &Arc<Self>, token: Value, done: Arc<AtomicBool>) -> JoinHandle<()> {
        let me = self.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut next = started + me.progress_every;
            let mut n = 0u64;
            while !done.load(Ordering::SeqCst) {
                if Instant::now() >= next {
                    n += 1;
                    next += me.progress_every;
                    me.send(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {
                        "progressToken": token, "progress": n,
                        "message": format!("waiting for ChatGPT ({}s)", started.elapsed().as_secs()),
                    }}));
                }
                std::thread::sleep(Duration::from_millis(20).min(me.progress_every));
            }
        })
    }

    fn ask(self: &Arc<Self>, id: Value, args: Map<String, Value>, progress: Option<Value>) -> Option<JoinHandle<()>> {
        let request_id = self.request_id(&id, &args)?;
        let mut channel = self.defaults.clone();
        if let Some(m) = args.get("model").and_then(Value::as_str) {
            channel.model = Some(m.into());
        }
        if let Some(p) = args.get("project").and_then(Value::as_str) {
            channel.project = p.into();
        }
        if let Some(t) = args.get("timeout_secs").and_then(Value::as_u64) {
            channel.timeout = t;
        }
        let opts = channel_options(&channel);
        let token = new_owner_token(self.seq.fetch_add(1, Ordering::SeqCst));
        let input = AskInput {
            prompt: args["prompt"].as_str().unwrap_or_default().to_string(),
            files: args
                .get("files")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
            context: args.get("context").and_then(Value::as_str).map(str::to_string),
            schema: args.get("output_schema").cloned().map(SchemaSource::Inline),
            request_id: Some(request_id.clone()),
            owner: Some(receipt::OWNER_MCP),
            owner_token: Some(token.clone()),
        };
        self.spawn_turn(id, progress, request_id, token, move |backend, flag| backend.ask(input, opts, flag))
    }

    fn resume(self: &Arc<Self>, id: Value, args: Map<String, Value>, progress: Option<Value>) -> Option<JoinHandle<()>> {
        let request_id = args["request_id"].as_str().unwrap_or_default().to_string();
        if !receipt::valid_id(&request_id) {
            self.error(&id, -32602, format!("invalid request_id {request_id:?}"));
            return None;
        }
        let schema = args.get("output_schema").cloned();
        let channel = self.defaults.clone();
        let rid = request_id.clone();
        let token = new_owner_token(self.seq.fetch_add(1, Ordering::SeqCst));
        let tok = token.clone();
        self.spawn_turn(id, progress, request_id, token, move |backend, flag| {
            backend.resume(&rid, schema, channel, &tok, flag)
        })
    }

    fn status(&self, id: Value, args: &Map<String, Value>) {
        let request_id = args["request_id"].as_str().unwrap_or_default();
        let mut envelope = self.backend.status(request_id);
        let running_here = self.state.lock().unwrap().tasks.contains_key(request_id);
        if running_here && envelope.get("state").is_none() {
            // Running here, before its receipt exists.
            envelope = json!({"request_id": request_id, "state": "running", "submitted": "unknown"});
        }
        let ok = envelope.get("state").is_some();
        self.reply(&id, tool_result(envelope, ok));
    }

    fn cancel(self: &Arc<Self>, id: Value, args: &Map<String, Value>) -> Option<JoinHandle<()>> {
        let request_id = args["request_id"].as_str().unwrap_or_default().to_string();
        let flag = self.state.lock().unwrap().tasks.get(&request_id).map(|t| t.flag.clone());
        let me = self.clone();
        Some(std::thread::spawn(move || {
            let mut envelope = match flag {
                // Ours: raise its flag and wait for its worker to finish.
                Some(flag) => {
                    flag.store(true, Ordering::SeqCst);
                    let deadline = Instant::now() + CANCEL_WAIT;
                    let mut finished = None;
                    while Instant::now() < deadline {
                        let state = me.state.lock().unwrap();
                        if !state.tasks.contains_key(&request_id) {
                            finished = state.finished.get(&request_id).cloned();
                            break;
                        }
                        drop(state);
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    match finished.as_deref() {
                        None => json!({"status": "cancel_requested",
                                       "message": "stop was requested; the request has not finished yet"}),
                        Some(outcome) => json!({
                            "status": crate::cmd::cancel::outcome_status(Some(outcome)),
                            "outcome": outcome,
                        }),
                    }
                }
                // Not ours: the CLI's cancel, which knows every other owner.
                None => me.backend.cancel(&request_id, me.defaults.clone()),
            };
            envelope["request_id"] = request_id.as_str().into();
            let ok = matches!(envelope["status"].as_str(), Some("cancelled" | "already_finished"));
            me.reply(&id, tool_result(envelope, ok));
        }))
    }

    /// The caller's request id, or a fresh one. Replies with an error and
    /// returns None when the caller's is unusable.
    fn request_id(&self, id: &Value, args: &Map<String, Value>) -> Option<String> {
        match args.get("request_id").and_then(Value::as_str) {
            Some(r) if receipt::valid_id(r) => Some(r.to_string()),
            Some(r) => {
                self.error(id, -32602, format!("invalid request_id {r:?}: letters, digits, '.', '_' or '-' (up to 128)"));
                None
            }
            None => {
                let n = self.seq.fetch_add(1, Ordering::SeqCst);
                Some(format!("mcp-{}-{}-{n}", std::process::id(), crate::throttle::now_secs()))
            }
        }
    }

    /// Stop every running request and wait up to `within` for them to end.
    pub fn shutdown(&self, within: Duration) {
        for task in self.state.lock().unwrap().tasks.values() {
            task.flag.store(true, Ordering::SeqCst);
        }
        let deadline = Instant::now() + within;
        while Instant::now() < deadline && !self.state.lock().unwrap().tasks.is_empty() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// One inbound JSON-RPC message, checked before anything acts on it.
#[derive(Debug, PartialEq)]
enum Inbound {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
    Response,
    /// Answered with -32600, under `id` when it was usable, else null.
    Invalid { id: Value, why: &'static str },
}

fn classify(msg: &Value) -> Inbound {
    let invalid = |id: Value, why| Inbound::Invalid { id, why };
    let Some(obj) = msg.as_object() else {
        // Batches were removed from MCP in 2025-06-18; scalars were never valid.
        return invalid(Value::Null, "a message must be one JSON object");
    };
    // The id must be a string or an integer to be echoed back at all. MCP
    // forbids null, and an object or float id cannot identify a request.
    let id = match obj.get("id") {
        None => None,
        Some(v) if v.is_string() || v.is_i64() || v.is_u64() => Some(v.clone()),
        Some(_) => return invalid(Value::Null, "id must be a string or an integer"),
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return invalid(id.unwrap_or(Value::Null), "jsonrpc must be \"2.0\"");
    }
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    if !(params.is_null() || params.is_object()) {
        return invalid(id.unwrap_or(Value::Null), "params must be an object");
    }
    match (obj.get("method"), id) {
        (Some(Value::String(m)), Some(id)) => Inbound::Request { id, method: m.clone(), params },
        (Some(Value::String(m)), None) => Inbound::Notification { method: m.clone(), params },
        (Some(_), id) => invalid(id.unwrap_or(Value::Null), "method must be a string"),
        (None, Some(_)) if obj.contains_key("result") || obj.contains_key("error") => Inbound::Response,
        (None, id) => invalid(id.unwrap_or(Value::Null), "no method"),
    }
}

/// Raise `flag` when this run's own marker appears (a shell's `chatgpt-use
/// cancel` aimed at it), until `done`.
fn watch_marker(path: std::path::PathBuf, flag: Arc<AtomicBool>, done: Arc<AtomicBool>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while !done.load(Ordering::SeqCst) {
            if path.exists() {
                flag.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    })
}

/// After a panic, close `request_id`'s receipt with `envelope`, but only if
/// this run owns it: a refused duplicate must not settle another's request.
fn finish_if_ours(request_id: &str, owner_token: &str, envelope: &Value) {
    let path = receipt::path_for(request_id);
    if receipt::load(&path).is_some_and(|r| r.owner_token.as_deref() == Some(owner_token)) {
        receipt::finish(&path, envelope);
    }
}

/// A unique token for one run of one request on this server.
fn new_owner_token(seq: u64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}-{seq}", std::process::id())
}

fn channel_options(c: &ChannelArgs) -> ChannelOptions {
    ChannelOptions {
        profile: c.profile.clone(),
        session: c.session.clone(),
        project: c.project.clone(),
        timeout_secs: c.timeout,
        model: c.model.clone(),
        // A tool call must not hang behind another process's run.
        busy_fail: true,
        receipt: None,
        ignore_cooldown: false,
    }
}

fn initialize_result(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "chatgpt-use", "version": env!("CARGO_PKG_VERSION")},
        "instructions": "Ask the user's ChatGPT web subscription (Plus/Pro) through their logged-in \
            browser. One ask at a time; a second returns status busy. Every result is an envelope \
            with a status; only status completed carries an answer. Long turns: pass a request_id, \
            and if the call is lost use status / resume with it instead of asking again.",
    })
}

/// A tools/call result carrying `envelope` as both text and structured content.
fn tool_result(envelope: Value, ok: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": envelope.to_string()}],
        "structuredContent": envelope,
        "isError": !ok,
    })
}

fn tool_list() -> Vec<Value> {
    let request_id = json!({"type": "string", "description": "Caller-chosen id: letters, digits, '.', '_' or '-' (up to 128)."});
    let schema = json!({"type": "object", "description": "A self-contained JSON Schema; the reply must be one JSON value that validates against it."});
    vec![
        json!({
            "name": "ask",
            "description": "Send one prompt to ChatGPT in the user's browser and wait for the reply. \
                Returns an envelope: status completed with result.text (or the validated value when \
                output_schema is given), or a failure status with error.kind and error.submitted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "minLength": 1},
                    "context": {"type": "string", "description": "Context text sent before the prompt."},
                    "files": {"type": "array", "items": {"type": "string"}, "description": "Paths whose contents are sent as context."},
                    "output_schema": schema,
                    "request_id": request_id,
                    "model": {"type": "string", "description": "instant | medium | high | extra high | pro, or a model name."},
                    "project": {"type": "string", "description": "ChatGPT Project to file the chat under; empty for none."},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 3600},
                },
                "required": ["prompt"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "status",
            "description": "Report a request from its receipt without touching the browser.",
            "inputSchema": {"type": "object", "properties": {"request_id": request_id},
                            "required": ["request_id"], "additionalProperties": false},
        }),
        json!({
            "name": "resume",
            "description": "Wait for the reply of a request whose caller lost it, without sending anything again.",
            "inputSchema": {"type": "object", "properties": {"request_id": request_id, "output_schema": schema},
                            "required": ["request_id"], "additionalProperties": false},
        }),
        json!({
            "name": "cancel",
            "description": "Stop a request's generation. Reports cancelled only once the conversation record confirms it.",
            "inputSchema": {"type": "object", "properties": {"request_id": request_id},
                            "required": ["request_id"], "additionalProperties": false},
        }),
    ]
}

/// Strict argument check against a tool's input schema: required keys, no
/// unknown keys, and each value's JSON type and bounds.
fn check_args(schema: &Value, args: &Map<String, Value>) -> Result<(), String> {
    let props = schema["properties"].as_object().cloned().unwrap_or_default();
    for key in args.keys() {
        if !props.contains_key(key) {
            return Err(format!("unknown argument {key:?}"));
        }
    }
    for req in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !args.contains_key(req) {
            return Err(format!("missing required argument {req:?}"));
        }
    }
    for (key, value) in args {
        let spec = &props[key];
        let ok = match spec["type"].as_str() {
            Some("string") => value
                .as_str()
                .is_some_and(|s| s.chars().count() as u64 >= spec["minLength"].as_u64().unwrap_or(0)),
            Some("integer") => value.as_u64().is_some_and(|n| {
                n >= spec["minimum"].as_u64().unwrap_or(0)
                    && n <= spec["maximum"].as_u64().unwrap_or(u64::MAX)
            }),
            Some("array") => value.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
            Some("object") => value.is_object(),
            _ => true,
        };
        if !ok {
            return Err(format!("argument {key:?} must be {}", describe(spec)));
        }
    }
    Ok(())
}

fn describe(spec: &Value) -> String {
    match spec["type"].as_str() {
        Some("string") if spec.get("minLength").is_some() => "a non-empty string".into(),
        Some("integer") => format!(
            "an integer from {} to {}",
            spec["minimum"].as_u64().unwrap_or(0),
            spec["maximum"].as_u64().unwrap_or(u64::MAX)
        ),
        Some("array") => "an array of strings".into(),
        Some(t) => format!("of type {t}"),
        None => "valid".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::BusyPolicy;
    use std::sync::atomic::AtomicUsize;

    /// A backend with no browser: "long" runs until cancelled, anything else
    /// answers at once. Records how many asks it saw.
    #[derive(Default)]
    struct Fake {
        asks: AtomicUsize,
        /// The owner token of each ask, in order.
        tokens: Mutex<Vec<String>>,
    }

    impl Backend for Fake {
        fn ask(&self, input: AskInput, opts: ChannelOptions, cancel: Arc<AtomicBool>) -> Value {
            self.asks.fetch_add(1, Ordering::SeqCst);
            assert!(opts.busy_fail, "tool calls never queue behind another run");
            assert_eq!(input.owner, Some(receipt::OWNER_MCP));
            self.tokens.lock().unwrap().push(input.owner_token.clone().expect("every run carries its own token"));
            if input.prompt == "panic" {
                panic!("boom");
            }
            if input.prompt == "dup" {
                // What execute returns when another owner holds the id.
                return json!({"status": "duplicate", "error": {"kind": "duplicate_request",
                              "message": "already exists", "submitted": "unknown"}});
            }
            if input.prompt == "long" {
                let until = Instant::now() + Duration::from_secs(10);
                while Instant::now() < until {
                    if cancel.load(Ordering::SeqCst) {
                        return json!({"status": "cancelled",
                                      "error": {"kind": "cancelled", "message": "stopped", "submitted": "yes"}});
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                return json!({"status": "incomplete"});
            }
            assert!(!cancel.load(Ordering::SeqCst), "a fresh request starts uncancelled");
            json!({"status": "completed", "result": {"text": format!("echo:{}", input.prompt)},
                   "context": input.context})
        }
        fn status(&self, _: &str) -> Value {
            json!({"status": "failed", "error": {"kind": "unknown_request", "message": "no receipt", "submitted": "no"}})
        }
        fn resume(&self, _: &str, _: Option<Value>, _: ChannelArgs, token: &str, _: Arc<AtomicBool>) -> Value {
            assert!(!token.is_empty());
            json!({"status": "completed", "result": {"text": "resumed"}})
        }
        fn cancel(&self, _: &str, _: ChannelArgs) -> Value {
            json!({"status": "unknown", "error": {"kind": "no_conversation", "message": "elsewhere"}})
        }
    }

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Buf {
        fn messages(&self) -> Vec<Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).expect("stdout carries only JSON lines"))
                .collect()
        }
        fn response(&self, id: i64) -> Option<Value> {
            self.messages().into_iter().find(|m| m["id"] == id)
        }
        fn wait(&self, id: i64) -> Value {
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until {
                if let Some(m) = self.response(id) {
                    return m;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("no response to id {id}; got {:?}", self.messages());
        }
    }

    fn defaults() -> ChannelArgs {
        ChannelArgs {
            profile: "auto".into(),
            session: None,
            project: String::new(),
            timeout: 30,
            model: None,
            busy: BusyPolicy::Wait,
        }
    }

    fn test_marker(id: &str, token: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cgu-agent-mcp-markers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{id}.{token}.cancel"))
    }

    fn server() -> (Arc<Server<Fake>>, Buf) {
        server_with(PROGRESS_EVERY)
    }

    fn server_with(progress_every: Duration) -> (Arc<Server<Fake>>, Buf) {
        let buf = Buf::default();
        let base = Arc::into_inner(Server::new(Fake::default(), Box::new(buf.clone()), defaults())).unwrap();
        (Arc::new(Server { progress_every, marker_path: test_marker, ..base }), buf)
    }

    fn call(id: i64, name: &str, args: Value) -> String {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
               "params": {"name": name, "arguments": args}})
        .to_string()
    }

    fn envelope(msg: &Value) -> &Value {
        &msg["result"]["structuredContent"]
    }

    #[test]
    fn initialize_negotiates_the_protocol_version() {
        let (s, out) = server();
        s.handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#);
        s.handle(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#);
        assert_eq!(out.wait(1)["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(out.wait(2)["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert_eq!(out.wait(1)["result"]["capabilities"]["tools"]["listChanged"], false);
    }

    #[test]
    fn notifications_get_no_response() {
        let (s, out) = server();
        s.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        s.handle(r#"{"jsonrpc":"2.0","method":"no/such/notification"}"#);
        s.handle(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#);
        assert_eq!(out.wait(3)["result"], json!({}));
        assert_eq!(out.messages().len(), 1, "{:?}", out.messages());
    }

    #[test]
    fn protocol_errors_use_jsonrpc_codes() {
        let (s, out) = server();
        s.handle("{not json");
        s.handle(r#"{"jsonrpc":"2.0","id":4,"method":"no/such/method"}"#);
        s.handle(r#"{"jsonrpc":"1.0","id":5,"method":"ping"}"#);
        let msgs = out.messages();
        assert_eq!(msgs[0]["error"]["code"], -32700);
        assert_eq!(msgs[0]["id"], Value::Null);
        assert_eq!(out.wait(4)["error"]["code"], -32601);
        assert_eq!(out.wait(5)["error"]["code"], -32600);
    }

    #[test]
    fn tools_are_listed_with_strict_schemas() {
        let (s, out) = server();
        s.handle(r#"{"jsonrpc":"2.0","id":6,"method":"tools/list"}"#);
        let tools = out.wait(6)["result"]["tools"].as_array().unwrap().clone();
        let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["ask", "status", "resume", "cancel"]);
        for t in &tools {
            assert_eq!(t["inputSchema"]["additionalProperties"], false, "{t}");
        }
    }

    #[test]
    fn bad_arguments_are_rejected_before_anything_runs() {
        let (s, out) = server();
        s.handle(&call(7, "ask", json!({"prompt": "q", "stdin": "x"})));
        s.handle(&call(8, "ask", json!({})));
        s.handle(&call(9, "ask", json!({"prompt": ""})));
        s.handle(&call(10, "ask", json!({"prompt": "q", "timeout_secs": 0})));
        s.handle(&call(11, "ask", json!({"prompt": "q", "request_id": "../etc"})));
        s.handle(&call(12, "status", json!({"request_id": 5})));
        s.handle(&call(13, "nope", json!({})));
        s.handle(r#"{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"ask","arguments":[1]}}"#);
        for id in 7..=14 {
            assert_eq!(out.wait(id)["error"]["code"], -32602, "id {id}: {:?}", out.response(id));
        }
        assert_eq!(s.backend.asks.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ask_returns_the_envelope_as_structured_and_text_content() {
        let (s, out) = server();
        s.handle(&call(20, "ask", json!({"prompt": "hi", "context": "ctx", "request_id": "r-hi"})))
            .unwrap()
            .join()
            .unwrap();
        let msg = out.wait(20);
        let env = envelope(&msg);
        assert_eq!(env["status"], "completed");
        assert_eq!(env["result"]["text"], "echo:hi");
        assert_eq!(env["context"], "ctx", "MCP input comes from arguments");
        assert_eq!(env["request_id"], "r-hi");
        assert_eq!(msg["result"]["isError"], false);
        let text: Value = serde_json::from_str(msg["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(&text, env);
    }

    #[test]
    fn an_ask_without_an_id_gets_one_to_follow_up_with() {
        let (s, out) = server();
        s.handle(&call(21, "ask", json!({"prompt": "hi"}))).unwrap().join().unwrap();
        let id = envelope(&out.wait(21))["request_id"].as_str().unwrap().to_string();
        assert!(id.starts_with("mcp-") && receipt::valid_id(&id), "{id}");
    }

    #[test]
    fn a_long_ask_leaves_status_and_cancel_answerable_and_the_server_alive() {
        let (s, out) = server();
        let long = s.handle(&call(30, "ask", json!({"prompt": "long", "request_id": "r-long"}))).unwrap();

        // While it runs: status answers at once, a second turn is refused.
        s.handle(&call(31, "status", json!({"request_id": "r-long"})));
        let st = out.wait(31);
        assert_eq!(envelope(&st)["state"], "running");
        s.handle(&call(32, "ask", json!({"prompt": "other", "request_id": "r-other"})));
        let busy = out.wait(32);
        assert_eq!(envelope(&busy)["status"], "busy");
        assert_eq!(envelope(&busy)["error"]["submitted"], "no");
        assert_eq!(busy["result"]["isError"], true);
        s.handle(&call(33, "ask", json!({"prompt": "long", "request_id": "r-long"})));
        assert_eq!(envelope(&out.wait(33))["status"], "duplicate");
        assert!(out.response(30).is_none(), "the long ask is still running");

        // Cancel it: only it stops, and the cancel reports the confirmed end.
        s.handle(&call(34, "cancel", json!({"request_id": "r-long"}))).unwrap().join().unwrap();
        long.join().unwrap();
        let ask = out.wait(30);
        assert_eq!(envelope(&ask)["status"], "cancelled");
        assert_eq!(envelope(&ask)["request_id"], "r-long");
        let cancel = out.wait(34);
        assert_eq!(envelope(&cancel)["status"], "cancelled");
        assert_eq!(cancel["result"]["isError"], false);

        // The server lives on, and the next request starts uncancelled.
        s.handle(&call(35, "ask", json!({"prompt": "next"}))).unwrap().join().unwrap();
        assert_eq!(envelope(&out.wait(35))["result"]["text"], "echo:next");
        s.handle(r#"{"jsonrpc":"2.0","id":36,"method":"ping"}"#);
        assert_eq!(out.wait(36)["result"], json!({}));
    }

    #[test]
    fn cancelling_a_request_held_elsewhere_goes_to_the_cli_cancel() {
        let (s, out) = server();
        s.handle(&call(40, "cancel", json!({"request_id": "r-elsewhere"}))).unwrap().join().unwrap();
        let msg = out.wait(40);
        assert_eq!(envelope(&msg)["error"]["kind"], "no_conversation");
        assert_eq!(envelope(&msg)["request_id"], "r-elsewhere");
        assert_eq!(msg["result"]["isError"], true);
    }

    #[test]
    fn a_protocol_cancel_stops_the_call_and_suppresses_its_response() {
        let (s, out) = server();
        let long = s.handle(&call(50, "ask", json!({"prompt": "long", "request_id": "r-quiet"}))).unwrap();
        s.handle(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":50}}"#);
        long.join().unwrap();
        assert!(out.response(50).is_none(), "{:?}", out.messages());
        s.handle(&call(51, "ask", json!({"prompt": "after"}))).unwrap().join().unwrap();
        assert_eq!(envelope(&out.wait(51))["result"]["text"], "echo:after");
    }

    #[test]
    fn resume_runs_in_the_browser_slot_too() {
        let (s, out) = server();
        let long = s.handle(&call(60, "ask", json!({"prompt": "long", "request_id": "r-slot"}))).unwrap();
        s.handle(&call(61, "resume", json!({"request_id": "r-old"})));
        assert_eq!(envelope(&out.wait(61))["status"], "busy");
        s.handle(&call(62, "cancel", json!({"request_id": "r-slot"}))).unwrap().join().unwrap();
        long.join().unwrap();
        s.handle(&call(63, "resume", json!({"request_id": "r-old"}))).unwrap().join().unwrap();
        assert_eq!(envelope(&out.wait(63))["result"]["text"], "resumed");
    }

    #[test]
    fn a_long_ask_reports_progress_when_asked_and_stops_after() {
        let (s, buf) = server_with(Duration::from_millis(40));
        let msg = json!({"jsonrpc": "2.0", "id": 70, "method": "tools/call",
                         "params": {"name": "ask", "arguments": {"prompt": "long", "request_id": "r-prog"},
                                    "_meta": {"progressToken": "tok-1"}}});
        let long = s.handle(&msg.to_string()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        s.handle(&call(71, "cancel", json!({"request_id": "r-prog"}))).unwrap().join().unwrap();
        long.join().unwrap();
        let progress: Vec<Value> = buf.messages().into_iter()
            .filter(|m| m["method"] == "notifications/progress").collect();
        assert!(progress.len() >= 2, "{progress:?}");
        assert!(progress.iter().all(|p| p["params"]["progressToken"] == "tok-1" && p.get("id").is_none()));
        let counts: Vec<u64> = progress.iter().map(|p| p["params"]["progress"].as_u64().unwrap()).collect();
        assert!(counts.windows(2).all(|w| w[0] < w[1]), "progress increases: {counts:?}");
        // Nothing after the response.
        let msgs = buf.messages();
        let reply_at = msgs.iter().position(|m| m["id"] == 70).unwrap();
        assert!(msgs[reply_at..].iter().all(|m| m["method"] != "notifications/progress"));

        // Without a token, no progress at all.
        let (quiet, out) = server();
        quiet.handle(&call(72, "ask", json!({"prompt": "hi"}))).unwrap().join().unwrap();
        assert!(out.messages().iter().all(|m| m["method"] != "notifications/progress"));
    }

    #[test]
    fn malformed_messages_are_invalid_requests_before_anything_acts() {
        for bad in [json!(null), json!([]), json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}]),
                    json!(1), json!("ping"), json!(true)] {
            assert_eq!(classify(&bad), Inbound::Invalid { id: Value::Null, why: "a message must be one JSON object" }, "{bad}");
        }
        for id in [json!(null), json!({}), json!([]), json!(1.5), json!(true)] {
            let msg = json!({"jsonrpc": "2.0", "id": id, "method": "ping"});
            assert!(matches!(classify(&msg), Inbound::Invalid { id: Value::Null, .. }), "{msg}");
        }
        let wrong = json!({"jsonrpc": "1.0", "id": 5, "method": "ping"});
        assert!(matches!(classify(&wrong), Inbound::Invalid { id, .. } if id == 5));
        let note = json!({"jsonrpc": "1.0", "method": "notifications/cancelled", "params": {"requestId": 1}});
        assert!(matches!(classify(&note), Inbound::Invalid { id: Value::Null, .. }), "notifications are checked too");
        let params = json!({"jsonrpc": "2.0", "id": 6, "method": "ping", "params": [1]});
        assert!(matches!(classify(&params), Inbound::Invalid { id, .. } if id == 6));
        let method = json!({"jsonrpc": "2.0", "id": 7, "method": 3});
        assert!(matches!(classify(&method), Inbound::Invalid { id, .. } if id == 7));
        assert_eq!(classify(&json!({"jsonrpc": "2.0", "id": 8, "result": {}})), Inbound::Response);
        assert!(matches!(classify(&json!({"jsonrpc": "2.0", "id": "a", "method": "ping"})), Inbound::Request { .. }));
        assert!(matches!(classify(&json!({"jsonrpc": "2.0", "method": "x"})), Inbound::Notification { .. }));
    }

    #[test]
    fn the_server_answers_malformed_messages_with_32600() {
        let (s, out) = server();
        s.handle("null");
        s.handle("[]");
        s.handle(r#"{"jsonrpc":"2.0","id":{},"method":"ping"}"#);
        let msgs = out.messages();
        assert_eq!(msgs.len(), 3, "{msgs:?}");
        for m in &msgs {
            assert_eq!(m["error"]["code"], -32600, "{m}");
            assert_eq!(m["id"], Value::Null, "{m}");
            assert!(m.get("result").is_none());
        }
    }

    #[test]
    fn a_wrong_version_cancel_notification_cancels_nothing() {
        let (s, out) = server();
        let long = s.handle(&call(80, "ask", json!({"prompt": "long", "request_id": "r-ver"}))).unwrap();
        s.handle(r#"{"jsonrpc":"1.0","method":"notifications/cancelled","params":{"requestId":80}}"#);
        std::thread::sleep(Duration::from_millis(150));
        assert!(out.response(80).is_none(), "still running");
        s.handle(&call(81, "cancel", json!({"request_id": "r-ver"}))).unwrap().join().unwrap();
        long.join().unwrap();
        assert_eq!(envelope(&out.wait(80))["status"], "cancelled");
    }

    #[test]
    fn a_progress_token_must_be_a_string_or_number() {
        let (s, out) = server();
        let msg = json!({"jsonrpc": "2.0", "id": 85, "method": "tools/call",
                         "params": {"name": "ask", "arguments": {"prompt": "hi"}, "_meta": {"progressToken": {}}}});
        assert!(s.handle(&msg.to_string()).is_none());
        assert_eq!(out.wait(85)["error"]["code"], -32602);
        assert_eq!(s.backend.asks.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_panicking_worker_answers_frees_the_slot_and_claims_nothing_was_known() {
        let (s, out) = server();
        s.handle(&call(90, "ask", json!({"prompt": "panic", "request_id": "r-panic"}))).unwrap().join().unwrap();
        let msg = out.wait(90);
        let env = envelope(&msg);
        assert_eq!(env["status"], "failed");
        assert_eq!(env["error"]["kind"], "internal");
        assert_eq!(env["error"]["submitted"], "unknown", "a panic proves nothing about sending");
        assert_eq!(msg["result"]["isError"], true);
        s.handle(&call(91, "ask", json!({"prompt": "after"}))).unwrap().join().unwrap();
        assert_eq!(envelope(&out.wait(91))["result"]["text"], "echo:after", "not stuck busy");
    }

    #[test]
    fn each_run_watches_and_clears_only_its_own_marker() {
        // Two servers handle the same request id: A holds it, B is refused as
        // a duplicate. A third, unrelated marker for the id also exists.
        let (a, out_a) = server();
        let (b, out_b) = server();
        let stranger = test_marker("r-shared", "someone-else");
        std::fs::write(&stranger, b"").unwrap();

        let long = a.handle(&call(100, "ask", json!({"prompt": "long", "request_id": "r-shared"}))).unwrap();
        b.handle(&call(101, "ask", json!({"prompt": "dup", "request_id": "r-shared"}))).unwrap().join().unwrap();
        assert_eq!(envelope(&out_b.wait(101))["status"], "duplicate");
        assert!(stranger.exists(), "B left a marker that is not its own");
        std::thread::sleep(Duration::from_millis(400));
        assert!(out_a.response(100).is_none(), "nothing aimed at A's run, so A still runs");

        // The shell's cancel addresses A's run by its token: only A stops.
        let token_a = a.backend.tokens.lock().unwrap()[0].clone();
        let token_b = b.backend.tokens.lock().unwrap()[0].clone();
        assert_ne!(token_a, token_b, "every run has its own token");
        let mine = test_marker("r-shared", &token_a);
        std::fs::write(&mine, b"").unwrap();
        long.join().unwrap();
        assert_eq!(envelope(&out_a.wait(100))["status"], "cancelled");
        assert!(!mine.exists(), "A removes its own marker when done");
        assert!(stranger.exists(), "and nobody else's");
        let _ = std::fs::remove_file(&stranger);
    }

    #[test]
    fn owner_tokens_tell_servers_and_retries_apart() {
        let t: std::collections::HashSet<_> = (0..50).map(new_owner_token).collect();
        assert_eq!(t.len(), 50);
        assert!(t.iter().all(|t| t.starts_with(&format!("{}-", std::process::id()))));
        assert!(t.iter().all(|t| t.bytes().all(|b| b.is_ascii_digit() || b == b'-')), "safe in a file name");
    }

    #[test]
    fn tool_calls_never_queue_behind_another_process() {
        assert!(channel_options(&defaults()).busy_fail);
        assert!(matches!(defaults().busy, BusyPolicy::Wait), "even when the CLI default is to wait");
    }
}
