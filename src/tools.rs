//! Local tool executor — the "hands" in Mode 2 (and the tools Claude Code's
//! calls map onto in Mode 3). Reads/writes files and runs commands on THIS
//! machine, then hands observations back into the conversation. This is also
//! why ChatGPT gets "file access" without any tunnel: the bytes are read here.
//!
//! Owned by the CORE agent.

use crate::protocol::{ToolCall, ToolResult, ToolSpec};
use regex::Regex;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

// ---- Tool catalog -----------------------------------------------------------

/// The built-in tool catalog advertised to the model.
///
/// - read_file {path}            — read file contents (read-only, never prompts)
/// - write_file {path, content}  — write/overwrite a file (side-effecting)
/// - list_dir {path}             — directory listing (read-only, never prompts)
/// - grep {pattern, path?}       — search files with a regex (read-only)
/// - bash {command}              — run a shell command (side-effecting)
pub fn builtin_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read_file".to_string(),
            description: "Read the full contents of a file. Returns the file text.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file (relative to the working directory or absolute)."
                    }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "write_file".to_string(),
            description: "Write (create or overwrite) a file with the given content. \
                          Requires approval when auto-approve is off."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write (relative or absolute)."
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write into the file."
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: "list_dir".to_string(),
            description: "List files and directories under a path, recursively. \
                          Returns one entry per line."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path to list (relative or absolute). \
                                        Defaults to the working directory if omitted."
                    }
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "grep".to_string(),
            description: "Search files for lines matching a regular expression. \
                          Returns matching lines with file:line prefixes."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "A regular expression to search for."
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory path to search (relative or absolute). \
                                        Defaults to the working directory if omitted."
                    }
                },
                "required": ["pattern"]
            }),
        },
        ToolSpec {
            name: "bash".to_string(),
            description: "Run a shell command with `sh -c`. Returns stdout + stderr. \
                          Requires approval when auto-approve is off."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute."
                    }
                },
                "required": ["command"]
            }),
        },
    ]
}

// ---- Execution dispatcher ---------------------------------------------------

/// Execute one tool call against `cwd`.
///
/// `auto_approve == false` means side-effecting tools (write_file, bash) must
/// prompt on stderr/stdin for y/N before running; read-only tools never prompt.
/// Paths are resolved under `cwd`; errors are returned as `ok: false` results
/// rather than panicking.
pub fn execute(call: &ToolCall, cwd: &Path, auto_approve: bool) -> ToolResult {
    let result = match call.name.as_str() {
        "read_file" => tool_read_file(&call.input, cwd),
        "write_file" => tool_write_file(&call.input, cwd, auto_approve),
        "list_dir" => tool_list_dir(&call.input, cwd),
        "grep" => tool_grep(&call.input, cwd),
        "bash" => tool_bash(&call.input, cwd, auto_approve),
        other => Err(format!("unknown tool: {other}")),
    };

    match result {
        Ok(content) => ToolResult {
            id: call.id.clone(),
            ok: true,
            content,
        },
        Err(msg) => ToolResult {
            id: call.id.clone(),
            ok: false,
            content: msg,
        },
    }
}

// ---- Individual tools -------------------------------------------------------

fn tool_read_file(input: &Value, cwd: &Path) -> Result<String, String> {
    let path_str = require_string(input, "path")?;
    let path = resolve_path(cwd, &path_str)?;
    std::fs::read_to_string(&path)
        .map_err(|e| format!("read_file: {}: {e}", path.display()))
}

fn tool_write_file(input: &Value, cwd: &Path, auto_approve: bool) -> Result<String, String> {
    let path_str = require_string(input, "path")?;
    let content = require_string(input, "content")?;
    let path = resolve_path(cwd, &path_str)?;

    if !auto_approve {
        let approved = prompt_approval(&format!(
            "write_file: overwrite/create {:?} ({} bytes)",
            path,
            content.len()
        ));
        if !approved {
            return Err("write_file: denied by user".to_string());
        }
    }

    // Create parent directories if needed.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("write_file: creating parent dirs: {e}"))?;
    }

    std::fs::write(&path, content)
        .map_err(|e| format!("write_file: {}: {e}", path.display()))?;

    Ok(format!("written: {}", path.display()))
}

fn tool_list_dir(input: &Value, cwd: &Path) -> Result<String, String> {
    let path_str = input
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    let path = resolve_path(cwd, path_str)?;

    if !path.is_dir() {
        return Err(format!("list_dir: not a directory: {}", path.display()));
    }

    let mut lines: Vec<String> = Vec::new();
    for entry in WalkDir::new(&path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        // Show relative path from the listed root.
        let rel = entry.path().strip_prefix(&path).unwrap_or(entry.path());
        let display = rel.display().to_string();
        if display.is_empty() || display == "." {
            continue;
        }
        if entry.path().is_dir() {
            lines.push(format!("{}/", display));
        } else {
            lines.push(display);
        }
    }

    if lines.is_empty() {
        return Ok("(empty directory)".to_string());
    }

    lines.sort();
    Ok(lines.join("\n"))
}

fn tool_grep(input: &Value, cwd: &Path) -> Result<String, String> {
    let pattern_str = require_string(input, "pattern")?;
    let re = Regex::new(&pattern_str)
        .map_err(|e| format!("grep: invalid regex {:?}: {e}", pattern_str))?;

    let search_path_str = input
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    let search_path = resolve_path(cwd, search_path_str)?;

    let mut matches: Vec<String> = Vec::new();

    // Walk files under search_path (or read a single file directly).
    let entries: Box<dyn Iterator<Item = PathBuf>> = if search_path.is_file() {
        Box::new(std::iter::once(search_path.clone()))
    } else {
        Box::new(
            WalkDir::new(&search_path)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .map(|e| e.path().to_path_buf()),
        )
    };

    for file_path in entries {
        // Skip binary-looking files (no extension or non-UTF-8).
        let content = match std::fs::read(&file_path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let text = match std::str::from_utf8(&content) {
            Ok(s) => s,
            Err(_) => continue, // skip binary files
        };

        let rel = file_path
            .strip_prefix(cwd)
            .unwrap_or(&file_path)
            .display()
            .to_string();

        for (line_no, line) in text.lines().enumerate() {
            if re.is_match(line) {
                matches.push(format!("{}:{}: {}", rel, line_no + 1, line));
            }
        }
    }

    if matches.is_empty() {
        return Ok(format!("(no matches for {:?})", pattern_str));
    }

    Ok(matches.join("\n"))
}

fn tool_bash(input: &Value, cwd: &Path, auto_approve: bool) -> Result<String, String> {
    let command = require_string(input, "command")?;

    if !auto_approve {
        let approved = prompt_approval(&format!("bash: run command: {command}"));
        if !approved {
            return Err("bash: denied by user".to_string());
        }
    }

    let output = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("bash: failed to spawn shell: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);

    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("[stderr]\n");
        result.push_str(&stderr);
    }
    if exit_code != 0 {
        result.push_str(&format!("\n[exit code: {exit_code}]"));
    }

    if result.is_empty() {
        result = "(no output)".to_string();
    }

    Ok(result)
}

// ---- Helpers ----------------------------------------------------------------

/// Extract a required string field from a JSON input object.
fn require_string(input: &Value, key: &str) -> Result<String, String> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required string field: {key}"))
}

/// Resolve a path string against `cwd`, returning an absolute PathBuf.
/// Relative paths are joined to `cwd`.
/// Read-only tools — the set exposed under the `read-only` MCP profile.
pub fn is_read_only(tool_name: &str) -> bool {
    matches!(tool_name, "read_file" | "list_dir" | "grep")
}

/// Resolve a tool path inside the workspace, REJECTING anything that escapes it:
/// absolute paths, `..` traversal, and symlink escapes (the deepest existing
/// ancestor, canonicalized, must stay under the canonical workspace root). This
/// is the workspace sandbox for the publicly-exposed MCP channel.
fn resolve_path(cwd: &Path, path_str: &str) -> Result<PathBuf, String> {
    let req = Path::new(path_str);
    if req.is_absolute() {
        return Err(format!(
            "absolute paths are not allowed: {path_str:?} — use a workspace-relative path"
        ));
    }
    if req
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("path traversal with '..' is not allowed: {path_str:?}"));
    }
    let joined = cwd.join(req);
    let root = cwd
        .canonicalize()
        .map_err(|e| format!("cannot resolve workspace root: {e}"))?;
    // Canonicalize the deepest existing ancestor (the target itself may not exist
    // yet, e.g. write_file) and confirm it lives under the workspace root.
    let mut probe = joined.clone();
    let real = loop {
        match probe.canonicalize() {
            Ok(rp) => break rp,
            Err(_) => match probe.parent() {
                Some(par) if !par.as_os_str().is_empty() => probe = par.to_path_buf(),
                _ => return Err(format!("path escapes the workspace: {path_str:?}")),
            },
        }
    };
    if !real.starts_with(&root) {
        return Err(format!("path escapes the workspace root: {path_str:?}"));
    }
    Ok(joined)
}

/// Prompt on stderr + read from stdin; returns true if the user types y/Y.
fn prompt_approval(description: &str) -> bool {
    eprint!("[approval required] {description} — allow? [y/N] ");
    let _ = io::stderr().flush();

    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(_) => matches!(line.trim(), "y" | "Y"),
        Err(_) => false,
    }
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn make_call(id: &str, name: &str, input: Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }
    }

    fn tmpdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("chatgpt-use-test-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    // --- read_file ---

    #[test]
    fn read_file_roundtrip() {
        let dir = tmpdir();
        let file = dir.join("hello.txt");
        fs::write(&file, "hello, world\n").unwrap();

        let call = make_call("c1", "read_file", json!({"path": "hello.txt"}));
        let result = execute(&call, &dir, true);

        assert!(result.ok, "read_file should succeed: {:?}", result.content);
        assert!(result.content.contains("hello, world"));
    }

    #[test]
    fn read_file_missing() {
        let dir = tmpdir();
        let call = make_call("c1", "read_file", json!({"path": "no_such_file.txt"}));
        let result = execute(&call, &dir, true);
        assert!(!result.ok, "read_file on missing file should fail");
    }

    // --- write_file ---

    #[test]
    fn write_file_creates_and_reads_back() {
        let dir = tmpdir();
        let write_call = make_call(
            "c1",
            "write_file",
            json!({"path": "output.txt", "content": "written content"}),
        );
        let result = execute(&write_call, &dir, true);
        assert!(result.ok, "write_file should succeed: {:?}", result.content);

        let read_call = make_call("c2", "read_file", json!({"path": "output.txt"}));
        let read_result = execute(&read_call, &dir, true);
        assert!(read_result.ok);
        assert!(read_result.content.contains("written content"));
    }

    // --- list_dir ---

    #[test]
    fn list_dir_shows_files() {
        let dir = tmpdir();
        fs::write(dir.join("a.txt"), "a").unwrap();
        fs::write(dir.join("b.txt"), "b").unwrap();

        let call = make_call("c1", "list_dir", json!({"path": "."}));
        let result = execute(&call, &dir, true);
        assert!(result.ok, "list_dir should succeed: {:?}", result.content);
        assert!(result.content.contains("a.txt"));
        assert!(result.content.contains("b.txt"));
    }

    // --- grep ---

    #[test]
    fn grep_finds_matching_lines() {
        let dir = tmpdir();
        fs::write(dir.join("src.rs"), "fn hello() {}\nfn world() {}\n").unwrap();

        let call = make_call("c1", "grep", json!({"pattern": "hello", "path": "src.rs"}));
        let result = execute(&call, &dir, true);
        assert!(result.ok, "grep should succeed: {:?}", result.content);
        assert!(result.content.contains("hello"));
        assert!(!result.content.contains("world"), "should not match 'world'");
    }

    #[test]
    fn grep_no_matches() {
        let dir = tmpdir();
        fs::write(dir.join("empty.rs"), "fn foo() {}").unwrap();

        let call = make_call("c1", "grep", json!({"pattern": "XYZZY_NOT_FOUND", "path": "."}));
        let result = execute(&call, &dir, true);
        assert!(result.ok);
        assert!(result.content.contains("no matches"));
    }

    #[test]
    fn grep_invalid_regex_returns_error() {
        let dir = tmpdir();
        let call = make_call("c1", "grep", json!({"pattern": "[invalid"}));
        let result = execute(&call, &dir, true);
        assert!(!result.ok);
        assert!(result.content.contains("invalid regex"));
    }

    // --- bash ---

    #[test]
    fn bash_runs_command_auto_approved() {
        let dir = tmpdir();
        let call = make_call("c1", "bash", json!({"command": "echo 'hello from bash'"}));
        let result = execute(&call, &dir, true);
        assert!(result.ok, "bash should succeed: {:?}", result.content);
        assert!(result.content.contains("hello from bash"));
    }

    #[test]
    fn bash_captures_exit_code_on_failure() {
        let dir = tmpdir();
        let call = make_call("c1", "bash", json!({"command": "exit 42"}));
        let result = execute(&call, &dir, true);
        // ok can be true (we ran it) but the content should mention the exit code.
        assert!(result.content.contains("42"), "should report non-zero exit code");
    }

    // --- unknown tool ---

    #[test]
    fn unknown_tool_returns_error() {
        let dir = tmpdir();
        let call = make_call("c1", "no_such_tool", json!({}));
        let result = execute(&call, &dir, true);
        assert!(!result.ok);
        assert!(result.content.contains("unknown tool"));
    }

    // --- builtin_specs ---

    #[test]
    fn builtin_specs_includes_all_five_tools() {
        let specs = builtin_specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"list_dir"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"bash"));
    }
}
