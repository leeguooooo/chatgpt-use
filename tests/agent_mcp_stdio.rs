//! `agent-mcp` end to end through the real binary, offline: a temp home holds
//! an account cooldown, so `ask` is refused before any browser is touched, and
//! PATH has no chrome-use either way. Checks the stdio contract a calling
//! agent depends on: JSON-RPC only on stdout, the shared envelope, and a
//! receipt that `status` reads back.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_home() -> PathBuf {
    let d = std::env::temp_dir().join(format!("chatgpt-use-agent-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let state = d.join(".chatgpt-use");
    std::fs::create_dir_all(&state).unwrap();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    std::fs::write(
        state.join("throttle.json"),
        format!(r#"{{"until": {}, "last_hit": {now}, "hits": 1}}"#, now + 600),
    )
    .unwrap();
    d
}

#[test]
fn agent_mcp_speaks_jsonrpc_on_stdout_and_shares_the_ask_envelope() {
    let home = temp_home();
    let mut child = Command::new(env!("CARGO_BIN_EXE_chatgpt-use"))
        .args(["agent-mcp", "--project", ""])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("PATH", "/usr/bin:/bin")
        .env("CHATGPT_USE_NO_UPDATE_CHECK", "1")
        .env_remove("CHATGPT_USE_IGNORE_COOLDOWN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let send = |stdin: &mut std::process::ChildStdin, v: Value| writeln!(stdin, "{v}").unwrap();
    let mut recv = || -> Value {
        let line = lines.next().expect("a response line").unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON on stdout: {line:?} ({e})"))
    };

    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                           "clientInfo": {"name": "test", "version": "0"}}}));
    let init = recv();
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "chatgpt-use");
    send(&mut stdin, json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = recv();
    assert_eq!(tools["id"], 2, "the notification got no response");
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 4);

    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "ask", "arguments": {"prompt": "hi", "request_id": "it-1"}}}));
    let ask = recv();
    assert_eq!(ask["id"], 3);
    let env = &ask["result"]["structuredContent"];
    assert_eq!(env["status"], "unavailable", "{ask}");
    assert_eq!(env["error"]["kind"], "rate_limited");
    assert_eq!(env["error"]["submitted"], "no");
    assert_eq!(env["request_id"], "it-1");
    assert_eq!(ask["result"]["isError"], true);

    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {"name": "status", "arguments": {"request_id": "it-1"}}}));
    let status = recv();
    let st = &status["result"]["structuredContent"];
    assert_eq!(st["state"], "failed", "{status}");
    assert_eq!(st["outcome"], "unavailable");
    assert_eq!(st["submitted"], "no");

    // The receipt names the server as owner, so a shell cancel uses the marker.
    let receipt: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".chatgpt-use/requests/it-1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["owner"], "mcp");

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "the server exits cleanly when its client hangs up");
    let _ = std::fs::remove_dir_all(&home);
}
