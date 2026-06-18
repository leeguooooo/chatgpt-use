//! MCP channel — a local MCP server that exposes this project's tools
//! (read_file / write_file / list_dir / grep / bash, from `crate::tools`) to a
//! *regular* GPT-5.5 conversation. Regular GPT-5.5 supports native MCP tool-calling,
//! so this is the no-role-play path (the browser channel can't do tools; this can).
//!
//! **Setup**: expose via a public tunnel (Cloudflare Tunnel / ngrok / Tailscale)
//! then register the public URL in ChatGPT > Settings > Apps as a custom MCP
//! connector. Copy `--token` into the connector header as `Authorization: Bearer
//! <token>`. NOTE: GPT-5.5 Pro cannot use MCP connectors — this channel targets
//! the regular GPT-5.5 tier only.
//!
//! **Transport**: plain HTTP JSON-RPC 2.0 on `POST /` (or `POST /mcp`).
//! SSE transport is not required for a first cut; add it later if ChatGPT requires
//! streaming. The server handles requests serially — fine for an operator-local
//! single-user setup.
//!
//! **JSON-RPC methods implemented**:
//!   - `initialize`              → server capabilities + serverInfo
//!   - `notifications/initialized` → no-op (notification; no response sent)
//!   - `tools/list`              → MCP tool descriptors for every builtin spec
//!   - `tools/call`              → dispatch to `crate::tools::execute` (auto_approve=true)
//!
//! Owned by the MCP agent.

use crate::cli::McpArgs;
use crate::protocol::ToolCall;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

// ---- Request-id counter -----------------------------------------------------

/// Monotonically increasing counter used to generate unique tool-call ids.
/// No RNG needed; a static prefix + counter is deterministic and sufficient.
static CALL_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_call_id() -> String {
    let n = CALL_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("mcp_call_{n}")
}

// ---- Auth helpers -----------------------------------------------------------

/// Check the `Authorization: Bearer <token>` header or `?token=<token>` query
/// parameter against the configured shared secret.
///
/// Returns `true` if auth is satisfied (i.e. no token configured, or the
/// provided value matches). Uses `==` on `&str` slices, which is not perfectly
/// constant-time on all platforms, but avoids early-return short-circuits that
/// would be an obvious timing oracle — good enough for a local tunnel gate.
fn auth_ok(request: &tiny_http::Request, expected: &str) -> bool {
    // Check Authorization header first.
    for header in request.headers() {
        if header.field.equiv("Authorization") {
            let val = header.value.as_str();
            if let Some(bearer) = val.strip_prefix("Bearer ") {
                return bearer == expected;
            }
            // Malformed Authorization header — fail.
            return false;
        }
    }

    // Fall back to ?token= query parameter.
    let url = request.url();
    if let Some(query) = url.split('?').nth(1) {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("token=") {
                return value == expected;
            }
        }
    }

    false
}

// ---- JSON-RPC helpers -------------------------------------------------------

/// A parsed JSON-RPC 2.0 request. `id` is None for notifications.
#[derive(Debug)]
struct JsonRpcRequest {
    id: Option<Value>,
    method: String,
    params: Value,
}

/// Parse the raw body bytes into a `JsonRpcRequest`.
/// Returns `Err` with a JSON-RPC parse-error response body on failure.
fn parse_jsonrpc(body: &str) -> std::result::Result<JsonRpcRequest, Value> {
    let v: Value = serde_json::from_str(body).map_err(|e| {
        json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32700, "message": format!("parse error: {e}") }
        })
    })?;

    let method = v
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| {
            json!({
                "jsonrpc": "2.0",
                "id": v.get("id").cloned().unwrap_or(Value::Null),
                "error": { "code": -32600, "message": "invalid request: missing method" }
            })
        })?
        .to_string();

    let id = v.get("id").cloned();
    let params = v.get("params").cloned().unwrap_or(Value::Null);

    Ok(JsonRpcRequest { id, method, params })
}

/// Build a JSON-RPC 2.0 success response.
fn ok_response(id: &Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.clone().unwrap_or(Value::Null),
        "result": result
    })
}

/// Build a JSON-RPC 2.0 error response.
fn err_response(id: &Option<Value>, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.clone().unwrap_or(Value::Null),
        "error": { "code": code, "message": message }
    })
}

// ---- MCP method handlers ----------------------------------------------------

/// `initialize` — advertise capabilities and server identity.
fn handle_initialize(id: &Option<Value>) -> Value {
    ok_response(
        id,
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "chatgpt-use",
                "version": "0.0.1"
            }
        }),
    )
}

/// `tools/list` — map `crate::tools::builtin_specs()` to MCP tool descriptors.
///
/// Our `ToolSpec` uses `input_schema`; the MCP spec calls the same field
/// `inputSchema` (camelCase). We rename it here.
fn handle_tools_list(id: &Option<Value>, read_only: bool) -> Value {
    let specs = crate::tools::builtin_specs();
    let tools: Vec<Value> = specs
        .into_iter()
        .filter(|s| !read_only || crate::tools::is_read_only(&s.name))
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "inputSchema": s.input_schema
            })
        })
        .collect();

    ok_response(id, json!({ "tools": tools }))
}

/// `tools/call` — dispatch to `crate::tools::execute` and return MCP content.
///
/// Response shape:
/// ```json
/// {
///   "content": [{ "type": "text", "text": "<result text>" }],
///   "isError": false
/// }
/// ```
fn handle_tools_call(id: &Option<Value>, params: &Value, cwd: &std::path::Path, read_only: bool) -> Value {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return err_response(id, -32602, "tools/call: missing required param 'name'");
        }
    };

    // Workspace-exposed safety: under the read-only profile, refuse write/exec tools.
    if read_only && !crate::tools::is_read_only(&name) {
        return ok_response(
            id,
            json!({
                "content": [{ "type": "text", "text":
                    format!("tool '{name}' is disabled: this MCP server runs in the read-only profile (read_file/list_dir/grep only). Restart with --profile full on a trusted, non-exposed setup to enable write_file/bash.") }],
                "isError": true
            }),
        );
    }

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    let call = ToolCall {
        id: next_call_id(),
        name,
        input: arguments,
    };

    let result = crate::tools::execute(&call, cwd, true /* auto_approve — no human in this loop */);

    ok_response(
        id,
        json!({
            "content": [{ "type": "text", "text": result.content }],
            "isError": !result.ok
        }),
    )
}

// ---- Request dispatch -------------------------------------------------------

/// Dispatch a single JSON-RPC request and return the response body, or `None`
/// for notifications (requests without an `id`).
fn dispatch(req: &JsonRpcRequest, cwd: &std::path::Path, read_only: bool) -> Option<Value> {
    let id = &req.id;

    // Notifications (no `id`) → process but return no response.
    let is_notification = id.is_none();

    let response = match req.method.as_str() {
        "initialize" => handle_initialize(id),

        "notifications/initialized" => {
            // No-op notification. Return early — no response for notifications.
            return None;
        }

        "tools/list" => handle_tools_list(id, read_only),

        "tools/call" => handle_tools_call(id, &req.params, cwd, read_only),

        other => err_response(id, -32601, &format!("method not found: {other}")),
    };

    if is_notification {
        None
    } else {
        Some(response)
    }
}

// ---- Public entry point -----------------------------------------------------

/// Start the MCP JSON-RPC server and block serving requests.
pub fn run(args: &McpArgs) -> Result<()> {
    let cwd: PathBuf = match &args.cwd {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir()?,
    };

    let read_only = matches!(args.profile, crate::cli::ToolProfile::ReadOnly);

    let bind_addr = format!("{}:{}", args.host, args.port);
    let server = tiny_http::Server::http(&bind_addr)
        .map_err(|e| anyhow::anyhow!("failed to bind MCP server on {bind_addr}: {e}"))?;

    eprintln!("[mcp] listening on http://{bind_addr}");
    eprintln!(
        "[mcp] cwd: {}",
        cwd.display()
    );
    eprintln!(
        "[mcp] profile: {} ({})",
        if read_only { "read-only" } else { "full" },
        if read_only {
            "read_file/list_dir/grep"
        } else {
            "ALL tools incl. write_file + bash — trusted/local only"
        }
    );
    if args.token.is_some() {
        eprintln!("[mcp] auth: Bearer token required");
    } else {
        eprintln!("[mcp] auth: NONE — consider --token when tunneling");
    }
    eprintln!("[mcp] tunnel hint: expose with  cloudflared tunnel --url http://{bind_addr}");
    eprintln!("[mcp]   then register the public URL in ChatGPT > Settings > Apps > Add custom connector");

    // Serve requests serially — fine for an operator-local single-user setup.
    loop {
        let mut request = server.recv()?;

        // Read the body before anything else (tiny_http::Request needs &mut for body).
        let mut body_buf = Vec::new();
        std::io::copy(request.as_reader(), &mut body_buf)?;
        let body_str = String::from_utf8_lossy(&body_buf);

        // Method + path check.
        let url_path: &str = {
            let url = request.url();
            // strip query string for comparison
            if let Some(idx) = url.find('?') {
                &url[..idx]
            } else {
                url
            }
        };

        // We need 'url_path' as an owned value since request.url() borrows request.
        let url_path = url_path.to_string();

        if request.method() != &tiny_http::Method::Post
            || (url_path != "/" && url_path != "/mcp")
        {
            let resp = tiny_http::Response::from_string(r#"{"error":"not found"}"#)
                .with_status_code(404)
                .with_header(
                    "Content-Type: application/json"
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                );
            let _ = request.respond(resp);
            continue;
        }

        // Auth gate.
        if let Some(ref expected) = args.token {
            if !auth_ok(&request, expected) {
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32000, "message": "unauthorized: invalid or missing token" }
                })
                .to_string();
                let resp = tiny_http::Response::from_string(body)
                    .with_status_code(401)
                    .with_header(
                        "Content-Type: application/json"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    );
                let _ = request.respond(resp);
                continue;
            }
        }

        // Parse JSON-RPC.
        let (status, response_body) = match parse_jsonrpc(&body_str) {
            Err(err_body) => (200u16, err_body.to_string()),
            Ok(rpc_req) => {
                match dispatch(&rpc_req, &cwd, read_only) {
                    None => {
                        // Notification — send empty 204.
                        let resp = tiny_http::Response::empty(204);
                        let _ = request.respond(resp);
                        continue;
                    }
                    Some(resp_value) => (200, resp_value.to_string()),
                }
            }
        };

        let resp = tiny_http::Response::from_string(response_body)
            .with_status_code(status)
            .with_header(
                "Content-Type: application/json"
                    .parse::<tiny_http::Header>()
                    .unwrap(),
            );
        let _ = request.respond(resp);
    }
}

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- parse_jsonrpc ---

    #[test]
    fn parse_valid_request_with_id() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let req = parse_jsonrpc(raw).expect("should parse");
        assert_eq!(req.method, "initialize");
        assert_eq!(req.id, Some(json!(1)));
    }

    #[test]
    fn parse_notification_has_no_id() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
        let req = parse_jsonrpc(raw).expect("should parse notification");
        assert_eq!(req.method, "notifications/initialized");
        assert!(req.id.is_none(), "notification should have no id");
    }

    #[test]
    fn parse_malformed_json_returns_error() {
        let raw = r#"{"jsonrpc":"2.0","id":1,BROKEN}"#;
        let err = parse_jsonrpc(raw).expect_err("should fail on malformed JSON");
        assert_eq!(err["error"]["code"], -32700);
    }

    #[test]
    fn parse_missing_method_returns_invalid_request() {
        let raw = r#"{"jsonrpc":"2.0","id":2,"params":{}}"#;
        let err = parse_jsonrpc(raw).expect_err("should fail without method");
        assert_eq!(err["error"]["code"], -32600);
    }

    // --- handle_initialize ---

    #[test]
    fn initialize_returns_correct_shape() {
        let id = Some(json!(42));
        let resp = handle_initialize(&id);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 42);
        assert_eq!(resp["result"]["serverInfo"]["name"], "chatgpt-use");
        assert_eq!(resp["result"]["serverInfo"]["version"], "0.0.1");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    // --- handle_tools_list ---

    #[test]
    fn tools_list_contains_all_builtins() {
        let id = Some(json!("req-1"));
        let resp = handle_tools_list(&id, false);
        let tools = resp["result"]["tools"].as_array().expect("tools should be array");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"list_dir"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"bash"));
    }

    #[test]
    fn read_only_profile_hides_and_blocks_write_tools() {
        // tools/list under read-only shows only read_file/list_dir/grep.
        let id = Some(json!(1));
        let resp = handle_tools_list(&id, true);
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"read_file") && names.contains(&"list_dir") && names.contains(&"grep"));
        assert!(!names.contains(&"write_file"), "write_file must be hidden in read-only");
        assert!(!names.contains(&"bash"), "bash must be hidden in read-only");

        // tools/call to a write tool under read-only is refused with isError.
        let params = json!({ "name": "bash", "arguments": { "command": "echo hi" } });
        let resp = handle_tools_call(&id, &params, &std::env::temp_dir(), true);
        assert_eq!(resp["result"]["isError"], json!(true));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("read-only"), "should explain the read-only profile: {text}");
    }

    #[test]
    fn tools_list_uses_input_schema_camel_case() {
        let id = Some(json!(1));
        let resp = handle_tools_list(&id, false);
        let tools = resp["result"]["tools"].as_array().unwrap();
        for tool in tools {
            assert!(
                tool.get("inputSchema").is_some(),
                "tool '{}' should have 'inputSchema' (camelCase)",
                tool["name"]
            );
            assert!(
                tool.get("input_schema").is_none(),
                "tool '{}' must NOT expose snake_case 'input_schema'",
                tool["name"]
            );
        }
    }

    // --- handle_tools_call ---

    #[test]
    fn tools_call_missing_name_returns_error() {
        let id = Some(json!(3));
        let params = json!({ "arguments": {} });
        let cwd = std::env::temp_dir();
        let resp = handle_tools_call(&id, &params, &cwd, false);
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn tools_call_unknown_tool_returns_is_error_true() {
        let id = Some(json!(4));
        let params = json!({ "name": "no_such_tool", "arguments": {} });
        let cwd = std::env::temp_dir();
        let resp = handle_tools_call(&id, &params, &cwd, false);
        // Unknown tool: tools::execute returns ok=false, which maps to isError=true.
        assert_eq!(resp["result"]["isError"], true);
        let content = &resp["result"]["content"][0];
        assert_eq!(content["type"], "text");
        assert!(content["text"].as_str().unwrap().contains("unknown tool"));
    }

    #[test]
    fn tools_call_read_file_succeeds() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("mcp-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("hello.txt"), "hello from mcp test").unwrap();

        let id = Some(json!(5));
        let params = json!({ "name": "read_file", "arguments": { "path": "hello.txt" } });
        let resp = handle_tools_call(&id, &params, &dir, false);

        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hello from mcp test"));
    }

    // --- dispatch ---

    #[test]
    fn dispatch_notification_returns_none() {
        let rpc = JsonRpcRequest {
            id: None,
            method: "notifications/initialized".to_string(),
            params: Value::Null,
        };
        let result = dispatch(&rpc, &std::env::temp_dir(), false);
        assert!(result.is_none(), "notifications should produce no response");
    }

    #[test]
    fn dispatch_unknown_method_returns_method_not_found() {
        let rpc = JsonRpcRequest {
            id: Some(json!(99)),
            method: "bogus/method".to_string(),
            params: Value::Null,
        };
        let result = dispatch(&rpc, &std::env::temp_dir(), false).unwrap();
        assert_eq!(result["error"]["code"], -32601);
    }

    // --- next_call_id ---

    #[test]
    fn call_ids_are_unique() {
        let a = next_call_id();
        let b = next_call_id();
        assert_ne!(a, b);
        assert!(a.starts_with("mcp_call_"));
    }
}
