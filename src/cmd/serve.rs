//! Mode 3 · drop-in model. A local Anthropic-compatible endpoint (tiny_http)
//! so Claude Code (ANTHROPIC_BASE_URL=http://host:port) spends no model tokens:
//! its model calls are served by the ChatGPT web subscription instead.
//!
//! Per POST /v1/messages:
//!   - translate Anthropic {system, messages[], tools[]} into a prompt + the
//!     protocol tool catalog
//!   - drive it through the ChatGPT web channel
//!   - protocol::parse_reply, then re-encode as an Anthropic response:
//!       Text  -> {content:[{type:"text",...}]}
//!       Tools -> {content:[{type:"tool_use",id,name,input}], stop_reason:"tool_use"}
//!   - emit the SSE event sequence Claude Code expects when stream:true
//!
//! This is the most fragile mode (giant prompts, concurrency 1, rate limits,
//! tool-schema fidelity). Aim for a correct single-turn PoC; mark experimental.
//!
//! Owned by the MODE-3 agent.

use crate::channel::{Channel, ChannelOptions};
use crate::cli::ServeArgs;
use crate::protocol::{self, Reply, ToolSpec};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Anthropic request shapes (deserialized from incoming POST /v1/messages)
// ---------------------------------------------------------------------------

/// The system field can be a bare string or an array of content blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SystemField {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Deserialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

/// A single content block in a message.
/// The `kind` fields capture the `type` tag for untagged deserialization; they
/// are matched on by serde but not read at runtime.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ContentBlock {
    // {type:"text", text:"..."}
    Text {
        #[serde(rename = "type")]
        kind: String,
        text: String,
    },
    // {type:"tool_use", id:"...", name:"...", input:{}}
    ToolUse {
        #[serde(rename = "type")]
        kind: String,
        id: String,
        name: String,
        input: Value,
    },
    // {type:"tool_result", tool_use_id:"...", content:...}
    ToolResult {
        #[serde(rename = "type")]
        kind: String,
        tool_use_id: String,
        content: ToolResultContent,
    },
}

/// tool_result content can be a bare string or an array of text blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ToolResultContent {
    Text(String),
    Blocks(Vec<Value>),
}

/// The `content` field of a message can be a bare string or an array of blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: MessageContent,
}

/// A tool definition as Claude Code sends it.
#[derive(Debug, Deserialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: Value,
}

/// The full /v1/messages request body.
#[derive(Debug, Deserialize)]
struct MessagesRequest {
    model: String,
    #[serde(default)]
    system: Option<SystemField>,
    messages: Vec<AnthropicMessage>,
    #[serde(default)]
    tools: Vec<AnthropicTool>,
    #[serde(default)]
    stream: bool,
}

// ---------------------------------------------------------------------------
// Monotonic message-id counter (avoids any RNG dependency)
// ---------------------------------------------------------------------------
static MSG_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_msg_id() -> String {
    let n = MSG_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("msg_{:016x}", n)
}

// ---------------------------------------------------------------------------
// Prompt construction: Anthropic request → single ChatGPT prompt string
// ---------------------------------------------------------------------------

fn extract_system_text(sys: &Option<SystemField>) -> String {
    match sys {
        None => String::new(),
        Some(SystemField::Text(t)) => t.clone(),
        Some(SystemField::Blocks(blocks)) => blocks
            .iter()
            .filter(|b| b.kind == "text")
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn render_tool_result_content(c: &ToolResultContent) -> String {
    match c {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn render_message_content(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => text.clone(),
                ContentBlock::ToolUse { name, input, id, .. } => {
                    // Render as readable text so ChatGPT understands it called a tool.
                    format!(
                        "[assistant called tool {} (id={})]\nInput: {}",
                        name,
                        id,
                        serde_json::to_string_pretty(input).unwrap_or_default()
                    )
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    // Render the tool observation so ChatGPT sees it as context.
                    format!(
                        "[tool result for id={}]\n{}",
                        tool_use_id,
                        render_tool_result_content(content)
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

/// Build the full prompt string to send to ChatGPT.
///
/// Strategy:
///   1. If there are tools, use protocol::system_prompt to prepend the tool
///      protocol instructions + catalog, using the last user message as the "task".
///   2. Append the human-readable system text (Claude Code's system prompt).
///   3. Append the full message transcript, role-prefixed.
///
/// Limitation (PoC): the whole transcript is re-sent every turn; the ChatGPT
/// conversation accumulates this on top of whatever it already has, so very long
/// sessions will hit context limits. The correct fix is to drive ChatGPT as a
/// fresh conversation per Anthropic "conversation" (stateless mapping). That is
/// left for future work.
fn build_prompt(req: &MessagesRequest) -> String {
    // Extract the task from the last user message (best-effort for system_prompt).
    let task = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| render_message_content(&m.content))
        .unwrap_or_else(|| "Respond to the conversation.".to_string());

    // Map Anthropic tools → protocol ToolSpecs.
    let tool_specs: Vec<ToolSpec> = req
        .tools
        .iter()
        .map(|t| ToolSpec {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
        })
        .collect();

    let mut parts: Vec<String> = Vec::new();

    // Tool protocol header (only when there are tools to advertise).
    if !tool_specs.is_empty() {
        parts.push(protocol::system_prompt(&tool_specs, &task));
    }

    // Claude Code's own system prompt text.
    let sys_text = extract_system_text(&req.system);
    if !sys_text.is_empty() {
        parts.push(format!("=== SYSTEM ===\n{}", sys_text));
    }

    // Full message transcript.
    parts.push("=== CONVERSATION ===".to_string());
    for msg in &req.messages {
        let role_label = match msg.role.as_str() {
            "assistant" => "ASSISTANT",
            _ => "USER",
        };
        let body = render_message_content(&msg.content);
        parts.push(format!("[{}]\n{}", role_label, body));
    }

    parts.join("\n\n")
}

// ---------------------------------------------------------------------------
// Anthropic response construction
// ---------------------------------------------------------------------------

fn make_response_json(msg_id: &str, model: &str, reply: &Reply) -> Value {
    match reply {
        Reply::Text(text) => {
            json!({
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [{"type": "text", "text": text}],
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            })
        }
        Reply::Tools(calls) => {
            let content: Vec<Value> = calls
                .iter()
                .map(|c| {
                    json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.name,
                        "input": c.input
                    })
                })
                .collect();
            json!({
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": content,
                "stop_reason": "tool_use",
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            })
        }
    }
}

// ---------------------------------------------------------------------------
// SSE streaming helpers
// ---------------------------------------------------------------------------

/// Write one SSE frame: `event: <type>\ndata: <json>\n\n`
fn sse_frame(event_type: &str, data: &Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        event_type,
        serde_json::to_string(data).unwrap_or_default()
    )
}

/// Emit the full Anthropic SSE event sequence for a completed reply.
///
/// SSE event types emitted (in order):
///   1. message_start
///   2. content_block_start    (per content block)
///   3. ping                   (after first block start, matches Claude API behaviour)
///   4. content_block_delta    (per content block — text_delta or input_json_delta)
///   5. content_block_stop     (per content block)
///   6. message_delta          (carries stop_reason)
///   7. message_stop
///
/// For PoC simplicity all events are written at once after the full reply is
/// computed — no real streaming from ChatGPT's side occurs here.
fn build_sse_body(msg_id: &str, model: &str, reply: &Reply) -> String {
    let mut out = String::new();

    // Derive content array and stop_reason from the reply.
    let (content_items, stop_reason) = match reply {
        Reply::Text(t) => (
            vec![json!({"type": "text", "text": t})],
            "end_turn",
        ),
        Reply::Tools(calls) => {
            let items: Vec<Value> = calls
                .iter()
                .map(|c| {
                    json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.name,
                        "input": c.input
                    })
                })
                .collect();
            (items, "tool_use")
        }
    };

    // 1. message_start
    out.push_str(&sse_frame(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        }),
    ));

    // 2–5. One block_start / ping / delta / stop per content item.
    for (i, item) in content_items.iter().enumerate() {
        let item_type = item["type"].as_str().unwrap_or("text");

        // content_block_start
        let block_start_payload = if item_type == "text" {
            json!({
                "type": "content_block_start",
                "index": i,
                "content_block": {"type": "text", "text": ""}
            })
        } else {
            // tool_use block
            json!({
                "type": "content_block_start",
                "index": i,
                "content_block": {
                    "type": "tool_use",
                    "id": item["id"],
                    "name": item["name"],
                    "input": {}
                }
            })
        };
        out.push_str(&sse_frame("content_block_start", &block_start_payload));

        // ping (once, after first block — mirrors real Claude API)
        if i == 0 {
            out.push_str(&sse_frame("ping", &json!({"type": "ping"})));
        }

        // content_block_delta
        let delta_payload = if item_type == "text" {
            json!({
                "type": "content_block_delta",
                "index": i,
                "delta": {
                    "type": "text_delta",
                    "text": item["text"]
                }
            })
        } else {
            // For tool_use, input_json_delta carries the full serialised input.
            json!({
                "type": "content_block_delta",
                "index": i,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": serde_json::to_string(&item["input"]).unwrap_or_default()
                }
            })
        };
        out.push_str(&sse_frame("content_block_delta", &delta_payload));

        // content_block_stop
        out.push_str(&sse_frame(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": i}),
        ));
    }

    // 6. message_delta
    out.push_str(&sse_frame(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason,
                "stop_sequence": null
            },
            "usage": {"output_tokens": 0}
        }),
    ));

    // 7. message_stop
    out.push_str(&sse_frame(
        "message_stop",
        &json!({"type": "message_stop"}),
    ));

    out
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

/// Return an Anthropic-shaped error body so Claude Code surfaces the message.
fn error_response(msg_id: &str, model: &str, err: &anyhow::Error) -> Value {
    // We return a 200 with a text block explaining the failure; this way
    // Claude Code shows it in the conversation rather than crashing its loop.
    json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{
            "type": "text",
            "text": format!("[chatgpt-use serve error] {:#}", err)
        }],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 0, "output_tokens": 0}
    })
}

fn error_sse_body(msg_id: &str, model: &str, err: &anyhow::Error) -> String {
    // For streaming callers, emit the error as a single text block SSE stream.
    let err_reply = Reply::Text(format!("[chatgpt-use serve error] {:#}", err));
    build_sse_body(msg_id, model, &err_reply)
}

// ---------------------------------------------------------------------------
// /v1/models stub
// ---------------------------------------------------------------------------

fn models_response() -> Value {
    json!({
        "object": "list",
        "data": [
            {
                "id": "chatgpt-use-web",
                "object": "model",
                "created": 1700000000,
                "owned_by": "chatgpt-use"
            }
        ]
    })
}

// ---------------------------------------------------------------------------
// Request handler
// ---------------------------------------------------------------------------

fn handle_request(
    request: tiny_http::Request,
    channel: &mut Channel,
) -> Result<()> {
    let method = request.method().to_string();
    let url = request.url().to_string();

    // Strip query string for routing.
    let path = url.split('?').next().unwrap_or(&url);

    match (method.as_str(), path) {
        // ----------------------------------------------------------------
        // GET /v1/models — tiny stub so Claude Code's startup check passes.
        // ----------------------------------------------------------------
        ("GET", "/v1/models") => {
            let body = serde_json::to_string(&models_response())?;
            let response = tiny_http::Response::from_string(body)
                .with_header(
                    "Content-Type: application/json"
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                )
                .with_status_code(200);
            request.respond(response)?;
        }

        // ----------------------------------------------------------------
        // POST /v1/messages — the main Anthropic shim.
        // ----------------------------------------------------------------
        ("POST", "/v1/messages") => {
            // Read and parse the request body.
            let mut body_bytes = Vec::new();
            let mut req = request;
            req.as_reader().read_to_end(&mut body_bytes)?;

            let msg_id = next_msg_id();

            let parsed: Result<MessagesRequest, _> = serde_json::from_slice(&body_bytes);
            let req_body = match parsed {
                Ok(r) => r,
                Err(e) => {
                    // Return a 400 with a JSON error body.
                    let err_body = json!({
                        "type": "error",
                        "error": {
                            "type": "invalid_request_error",
                            "message": format!("Failed to parse request body: {}", e)
                        }
                    });
                    let response = tiny_http::Response::from_string(
                        serde_json::to_string(&err_body)?,
                    )
                    .with_header(
                        "Content-Type: application/json"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    )
                    .with_status_code(400);
                    req.respond(response)?;
                    return Ok(());
                }
            };

            let model = req_body.model.clone();
            let stream = req_body.stream;

            // Build the prompt and send it through the ChatGPT channel.
            let prompt = build_prompt(&req_body);
            let reply_result = channel.send(&prompt).map(|text| protocol::parse_reply(&text));

            if stream {
                // --- SSE streaming response ---
                let sse_body = match reply_result {
                    Ok(reply) => build_sse_body(&msg_id, &model, &reply),
                    Err(e) => {
                        eprintln!("[serve] channel error: {:#}", e);
                        error_sse_body(&msg_id, &model, &e)
                    }
                };
                let response = tiny_http::Response::from_string(sse_body)
                    .with_header(
                        "Content-Type: text/event-stream"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    )
                    .with_header(
                        "Cache-Control: no-cache"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    )
                    .with_header(
                        "Connection: keep-alive"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    )
                    .with_status_code(200);
                req.respond(response)?;
            } else {
                // --- Plain JSON response ---
                let json_body = match reply_result {
                    Ok(reply) => make_response_json(&msg_id, &model, &reply),
                    Err(e) => {
                        eprintln!("[serve] channel error: {:#}", e);
                        error_response(&msg_id, &model, &e)
                    }
                };
                let response = tiny_http::Response::from_string(
                    serde_json::to_string(&json_body)?,
                )
                .with_header(
                    "Content-Type: application/json"
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                )
                .with_status_code(200);
                req.respond(response)?;
            }
        }

        // ----------------------------------------------------------------
        // Everything else — 404.
        // ----------------------------------------------------------------
        _ => {
            let err_body = json!({
                "type": "error",
                "error": {
                    "type": "not_found_error",
                    "message": format!("Unknown endpoint: {} {}", method, path)
                }
            });
            let response =
                tiny_http::Response::from_string(serde_json::to_string(&err_body)?)
                    .with_header(
                        "Content-Type: application/json"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    )
                    .with_status_code(404);
            request.respond(response)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(args: &ServeArgs) -> Result<()> {
    let addr = format!("{}:{}", args.host, args.port);
    let server = tiny_http::Server::http(&addr)
        .map_err(|e| anyhow::anyhow!("Failed to bind {}: {}", addr, e))?;

    eprintln!("[chatgpt-use serve] listening on http://{}", addr);
    eprintln!("[chatgpt-use serve] set ANTHROPIC_BASE_URL=http://{} in Claude Code", addr);
    eprintln!("[chatgpt-use serve] EXPERIMENTAL — concurrency 1, text tool-call protocol");

    // Connect one Channel up front and reuse it for all requests.
    // The web surface is concurrency-1, so requests are handled serially.
    let opts = ChannelOptions {
        profile: args.channel.profile.clone(),
        session: args.channel.session.clone(),
        project: args.channel.project.clone(),
        timeout_secs: args.channel.timeout,
        model: args.channel.model.clone(),
    };
    let mut channel = Channel::connect(&opts)?;

    eprintln!("[chatgpt-use serve] ChatGPT channel connected — ready");

    // Serve requests serially. The channel is not thread-safe (concurrency 1
    // by design — the ChatGPT web surface rate-limits hard on the shared tab).
    for request in server.incoming_requests() {
        if let Err(e) = handle_request(request, &mut channel) {
            eprintln!("[serve] request handler error: {:#}", e);
        }
    }

    Ok(())
}
