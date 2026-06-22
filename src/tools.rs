//! Local tool executor — the "hands" in Mode 2 (and the tools Claude Code's
//! calls map onto in Mode 3). Reads/writes files and runs commands on THIS
//! machine, then hands observations back into the conversation. This is also
//! why ChatGPT gets "file access" without any tunnel: the bytes are read here.
//!
//! Owned by the CORE agent.

use crate::cli::PermissionMode;
use crate::protocol::{ToolCall, ToolResult, ToolSpec};
use regex::Regex;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

/// Persistent-shell config. When set (by the MCP server at startup), `bash`
/// behaves like a real terminal: the working directory and exported environment
/// carry over between calls, and each command is bounded by a timeout so a hung
/// command can't freeze the single-threaded server. When unset (unit tests,
/// Mode-2 `run`), `bash` keeps its original stateless one-shot behavior.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// Directory holding the session state (cwd + exported env) + scratch files.
    pub state_dir: PathBuf,
    /// Per-command wall-clock limit in seconds; 0 = unlimited.
    pub timeout_secs: u64,
}

static SHELL_CFG: OnceLock<ShellConfig> = OnceLock::new();

/// Enable persistent-shell mode. Call ONCE at MCP-server startup (full profile).
/// Clears any stale session state so each server lifetime is a fresh terminal.
pub fn configure_shell(state_dir: PathBuf, timeout_secs: u64) {
    let _ = std::fs::create_dir_all(&state_dir);
    // Fresh session: drop carried-over cwd/env from a previous server run.
    let _ = std::fs::remove_file(state_dir.join("cwd"));
    let _ = std::fs::remove_file(state_dir.join("env"));
    let _ = SHELL_CFG.set(ShellConfig { state_dir, timeout_secs });
}

/// Root directory for skill discovery (`list_skills`/`read_skill`). Each skill is
/// `<dir>/<name>/SKILL.md` with YAML frontmatter (name + description). Configured
/// by the MCP server at startup; an EMPTY path disables skill discovery.
static SKILLS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Set the skills root. Pass an empty path to disable discovery.
pub fn configure_skills(dir: PathBuf) {
    let _ = SKILLS_DIR.set(dir);
}

/// The effective skills root: the configured one, else the conventional
/// `~/.claude/skills`. Returns None when discovery is explicitly disabled
/// (configured to an empty path).
fn skills_dir() -> Option<PathBuf> {
    match SKILLS_DIR.get() {
        Some(p) if p.as_os_str().is_empty() => None, // explicitly disabled
        Some(p) => Some(p.clone()),
        None => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".claude").join("skills")),
    }
}

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
            description: "Run a shell command on this machine, like a terminal. Returns \
                          stdout + stderr + exit code. When the server runs as a persistent \
                          shell session, the working directory and exported environment carry \
                          over between calls (so `cd somedir` then `ls` works, and `export X=1` \
                          persists). Use it for builds, tests, git, file ops — anything a real \
                          terminal can do."
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
        ToolSpec {
            name: "edit_file".to_string(),
            description: "Replace one exact occurrence of old_string with new_string in a file \
                          (the old_string must be unique). Side-effecting; full profile only."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File to edit (workspace-relative)." },
                    "old_string": { "type": "string", "description": "Exact text to replace (must occur exactly once)." },
                    "new_string": { "type": "string", "description": "Replacement text." }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        },
        ToolSpec {
            name: "git_status".to_string(),
            description: "Show `git status` (short, with branch) for the workspace.".to_string(),
            input_schema: json!({ "type": "object", "properties": {}, "required": [] }),
        },
        ToolSpec {
            name: "git_diff".to_string(),
            description: "Show the unstaged `git diff`, optionally limited to one path.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Optional path to diff (workspace-relative)." } },
                "required": []
            }),
        },
        ToolSpec {
            name: "git_log".to_string(),
            description: "Show the last 30 commits (`git log --oneline`).".to_string(),
            input_schema: json!({ "type": "object", "properties": {}, "required": [] }),
        },
        ToolSpec {
            name: "git_show".to_string(),
            description: "Show a commit/ref with `git show --stat` (default HEAD).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "rev": { "type": "string", "description": "Git revision/ref (default HEAD)." } },
                "required": []
            }),
        },
        ToolSpec {
            name: "git_blame".to_string(),
            description: "Show `git blame` for a file (workspace-relative).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "File to blame (workspace-relative)." } },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "list_skills".to_string(),
            description: "List the local agent SKILLS available on this machine (name + one-line \
                          description each). A skill is a reusable capability — usually a CLI plus \
                          instructions (e.g. browser automation, email, image generation, Feishu/Lark \
                          ops). To USE one: call read_skill to learn how, then run its commands with \
                          the bash tool."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {}, "required": [] }),
        },
        ToolSpec {
            name: "read_skill".to_string(),
            description: "Read a skill's full instructions (its SKILL.md) plus a listing of the files \
                          in the skill directory, so you can follow it and run the right commands via \
                          the bash tool. Use the skill name from list_skills."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Skill name (as shown by list_skills)." } },
                "required": ["name"]
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
pub fn execute(call: &ToolCall, cwd: &Path, auto_approve: bool, perm: PermissionMode) -> ToolResult {
    let result = match call.name.as_str() {
        "read_file" => tool_read_file(&call.input, cwd),
        "write_file" => tool_write_file(&call.input, cwd, auto_approve),
        "edit_file" => tool_edit_file(&call.input, cwd, auto_approve),
        "list_dir" => tool_list_dir(&call.input, cwd),
        "grep" => tool_grep(&call.input, cwd),
        "bash" => tool_bash(&call.input, cwd, auto_approve, perm),
        "git_status" => tool_git(&["status", "--short", "--branch"], cwd),
        "git_diff" => tool_git_diff(&call.input, cwd),
        "git_log" => tool_git(&["log", "--oneline", "-n", "30"], cwd),
        "git_show" => tool_git_show(&call.input, cwd),
        "git_blame" => tool_git_blame(&call.input, cwd),
        "list_skills" => tool_list_skills(),
        "read_skill" => tool_read_skill(&call.input),
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

/// Catastrophic, irreversible patterns blocked even in `trusted` mode.
fn destructive_reason(cmd: &str) -> Option<String> {
    let c = cmd.to_lowercase();
    let pats = [
        "rm -rf /", "rm -rf /*", "rm -rf ~", "rm -fr /", "rm -rf --no-preserve-root",
        "mkfs", "dd if=", ":(){", "> /dev/sd", "of=/dev/", "chmod -r 777 /", "chown -r",
    ];
    pats.iter()
        .find(|p| c.contains(**p))
        .map(|p| format!("destructive pattern {p:?}"))
}

/// Network-reaching commands, blocked in `safe` mode.
fn network_reason(cmd: &str) -> Option<String> {
    let tools = ["curl", "wget", "nc ", "ncat", "netcat", "ssh ", "scp ", "sftp", "telnet", "ftp "];
    // crude word-ish check on the command head + after pipes/&&/;
    let segments: Vec<&str> = cmd.split(|ch| ch == '|' || ch == ';' || ch == '&').collect();
    for seg in segments {
        let head = seg.trim_start();
        if let Some(t) = tools.iter().find(|t| head.starts_with(t.trim()) && (head.len() == t.trim().len() || head[t.trim().len()..].starts_with(' '))) {
            return Some(format!("network command {:?}", t.trim()));
        }
    }
    None
}

/// Gate a bash command per the permission mode. Returns Some(reason) if blocked.
fn gate_command(command: &str, perm: PermissionMode) -> Option<String> {
    match perm {
        PermissionMode::Dangerous => None,
        PermissionMode::Trusted => destructive_reason(command),
        PermissionMode::Safe => destructive_reason(command)
            .or_else(|| network_reason(command))
            .or_else(|| {
                if command.contains("$(") || command.contains('`') {
                    Some("command substitution ($(…) / backticks)".to_string())
                } else {
                    None
                }
            }),
    }
}

/// Env var names that look secret — filtered out (except in `dangerous` mode).
fn is_secret_env(name: &str) -> bool {
    let n = name.to_uppercase();
    ["SECRET", "TOKEN", "PASSWORD", "PASSWD", "APIKEY", "API_KEY", "CREDENTIAL",
     "PRIVATE_KEY", "ACCESS_KEY", "SESSION", "OPENAI", "ANTHROPIC", "AWS_"]
        .iter()
        .any(|p| n.contains(p))
}

fn tool_bash(input: &Value, cwd: &Path, auto_approve: bool, perm: PermissionMode) -> Result<String, String> {
    let command = require_string(input, "command")?;

    if let Some(reason) = gate_command(&command, perm) {
        return Err(format!(
            "bash: blocked by permission policy: {reason}. Re-run with --permission-mode trusted or dangerous to allow."
        ));
    }

    if !auto_approve {
        let approved = prompt_approval(&format!("bash: run command: {command}"));
        if !approved {
            return Err("bash: denied by user".to_string());
        }
    }

    match SHELL_CFG.get() {
        Some(cfg) => run_persistent(&command, cwd, perm, cfg),
        None => run_oneshot(&command, cwd, perm),
    }
}

/// Apply the secret-env filter to a Command unless we're in dangerous mode.
fn apply_env_filter(cmd: &mut Command, perm: PermissionMode) {
    if !matches!(perm, PermissionMode::Dangerous) {
        cmd.env_clear();
        for (k, v) in std::env::vars() {
            if !is_secret_env(&k) {
                cmd.env(k, v);
            }
        }
    }
}

/// Format captured output into the user-facing result string (shared shape).
fn format_output(stdout: &str, stderr: &str, exit_code: i32, note: Option<&str>) -> String {
    let mut result = String::new();
    if let Some(n) = note {
        result.push_str(n);
        result.push('\n');
    }
    if !stdout.is_empty() {
        result.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str("[stderr]\n");
        result.push_str(stderr);
    }
    if exit_code != 0 {
        result.push_str(&format!("\n[exit code: {exit_code}]"));
    }
    if result.is_empty() {
        result = "(no output)".to_string();
    }
    result
}

/// Original stateless behavior: one fresh `sh -c` at `cwd`, no timeout.
fn run_oneshot(command: &str, cwd: &Path, perm: PermissionMode) -> Result<String, String> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(cwd);
    apply_env_filter(&mut cmd, perm);
    let output = cmd
        .output()
        .map_err(|e| format!("bash: failed to spawn shell: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(format_output(&stdout, &stderr, output.status.code().unwrap_or(-1), None))
}

/// Persistent-terminal behavior: the working directory and exported env carry
/// over between calls, with a per-command timeout. Output is redirected to files
/// inside the state dir (no pipes → no buffer deadlock while we poll for the
/// timeout). On timeout the child is killed and partial output is returned.
fn run_persistent(
    command: &str,
    default_cwd: &Path,
    perm: PermissionMode,
    cfg: &ShellConfig,
) -> Result<String, String> {
    let dir = &cfg.state_dir;
    std::fs::create_dir_all(dir).map_err(|e| format!("bash: cannot create state dir: {e}"))?;
    let cwd_file = dir.join("cwd");
    let env_file = dir.join("env");
    let out_file = dir.join("out");
    let err_file = dir.join("err");
    // Best-effort clean of prior scratch so we never report stale output.
    let _ = std::fs::remove_file(&out_file);
    let _ = std::fs::remove_file(&err_file);

    let q = |p: &Path| p.to_string_lossy().replace('\'', "'\\''");
    // Wrapper: restore session cwd/env, run the (raw, unescaped) command with its
    // output redirected to files, then persist the resulting cwd + exported env.
    // The command is injected verbatim on its own lines — that's the whole point
    // (arbitrary shell). Fixed paths are single-quoted; they contain no quotes.
    let script = format!(
        "__d=\"$(cat '{cwd}' 2>/dev/null)\"\n\
         if [ -d \"$__d\" ]; then cd \"$__d\"; else cd '{def}'; fi\n\
         [ -f '{env}' ] && . '{env}' 2>/dev/null\n\
         {{\n{cmd}\n}} > '{out}' 2> '{err}'\n\
         __rc=$?\n\
         pwd > '{cwd}' 2>/dev/null\n\
         export -p > '{env}' 2>/dev/null\n\
         exit $__rc\n",
        cwd = q(&cwd_file),
        env = q(&env_file),
        out = q(&out_file),
        err = q(&err_file),
        def = q(default_cwd),
        cmd = command,
    );

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&script).current_dir(default_cwd);
    apply_env_filter(&mut cmd, perm);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("bash: failed to spawn shell: {e}"))?;

    // Poll for completion up to the timeout (0 = unlimited).
    let mut timed_out = false;
    let exit_code = if cfg.timeout_secs == 0 {
        child
            .wait()
            .map_err(|e| format!("bash: wait failed: {e}"))?
            .code()
            .unwrap_or(-1)
    } else {
        let deadline = Instant::now() + Duration::from_secs(cfg.timeout_secs);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code().unwrap_or(-1),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        timed_out = true;
                        break -1;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(format!("bash: wait failed: {e}")),
            }
        }
    };

    let stdout = std::fs::read_to_string(&out_file).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err_file).unwrap_or_default();
    let note = if timed_out {
        Some(format!(
            "[timed out after {}s — process killed; output is partial]",
            cfg.timeout_secs
        ))
    } else {
        None
    };
    Ok(format_output(&stdout, &stderr, exit_code, note.as_deref()))
}

// ---- Skill discovery --------------------------------------------------------

/// Pull `name` + `description` out of a SKILL.md YAML frontmatter block (the
/// leading `--- … ---`). Returns (name, description) best-effort.
fn parse_skill_frontmatter(md: &str, fallback_name: &str) -> (String, String) {
    let mut name = fallback_name.to_string();
    let mut desc = String::new();
    let trimmed = md.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            for line in rest[..end].lines() {
                if let Some(v) = line.strip_prefix("name:") {
                    name = v.trim().trim_matches(['"', '\'']).to_string();
                } else if let Some(v) = line.strip_prefix("description:") {
                    desc = v.trim().trim_matches(['"', '\'']).to_string();
                }
            }
        }
    }
    (name, desc)
}

/// `list_skills` — enumerate `<skills_dir>/<name>/SKILL.md`, returning each
/// skill's name + one-line description.
fn tool_list_skills() -> Result<String, String> {
    let dir = match skills_dir() {
        Some(d) => d,
        None => return Err("skill discovery is disabled on this server (--skills-dir \"\").".to_string()),
    };
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| format!("list_skills: cannot read skills dir {}: {e}", dir.display()))?;

    let mut skills: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let skill_md = entry.path().join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let fallback = entry.file_name().to_string_lossy().to_string();
        let head = read_head(&skill_md, 4096);
        let (name, desc) = parse_skill_frontmatter(&head, &fallback);
        skills.push((name, desc));
    }
    if skills.is_empty() {
        return Ok(format!("(no skills found under {})", dir.display()));
    }
    skills.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = format!("{} skills available (call read_skill <name> to learn how to use one):\n\n", skills.len());
    for (name, desc) in skills {
        out.push_str(&format!("- {name}: {desc}\n"));
    }
    Ok(out)
}

/// `read_skill {name}` — return the skill's full SKILL.md plus a listing of the
/// files in its directory (so the model can see helper scripts to run via bash).
fn tool_read_skill(input: &Value) -> Result<String, String> {
    let dir = match skills_dir() {
        Some(d) => d,
        None => return Err("skill discovery is disabled on this server (--skills-dir \"\").".to_string()),
    };
    let name = require_string(input, "name")?;
    // Guard against path escapes — skill names are single path segments.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(format!("read_skill: invalid skill name {name:?}"));
    }
    let skill_root = dir.join(&name);
    let skill_md = skill_root.join("SKILL.md");
    if !skill_md.is_file() {
        return Err(format!(
            "read_skill: no skill named {name:?} (no {}). Use list_skills to see what's available.",
            skill_md.display()
        ));
    }
    let content = std::fs::read_to_string(&skill_md)
        .map_err(|e| format!("read_skill: cannot read {}: {e}", skill_md.display()))?;

    // List files in the skill dir (names + sizes) so the model knows what
    // scripts/resources it can run/read via bash.
    let mut files: Vec<String> = Vec::new();
    for entry in WalkDir::new(&skill_root)
        .max_depth(3)
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_file() {
            if let Ok(rel) = entry.path().strip_prefix(&skill_root) {
                files.push(rel.to_string_lossy().to_string());
            }
        }
    }
    files.sort();

    Ok(format!(
        "# skill: {name}\n# location: {}\n\n## SKILL.md\n{content}\n\n## files in this skill ({} total)\n{}\n\n\
         To use this skill, run the relevant commands with the bash tool (paths above are relative to {}).",
        skill_root.display(),
        files.len(),
        files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n"),
        skill_root.display(),
    ))
}

/// Read up to `max` bytes from the start of a file (for frontmatter parsing).
fn read_head(path: &Path, max: usize) -> String {
    use std::io::Read as _;
    let mut buf = Vec::new();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(max as u64).read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
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
/// Targeted exact-string edit (apply_patch-style, but match-exact for safety).
/// Side-effecting → full profile only.
fn tool_edit_file(input: &Value, cwd: &Path, auto_approve: bool) -> Result<String, String> {
    let path_str = require_string(input, "path")?;
    let old = require_string(input, "old_string")?;
    let new = require_string(input, "new_string")?;
    let path = resolve_path(cwd, &path_str)?;
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("edit_file: {}: {e}", path.display()))?;
    let count = content.matches(&old).count();
    if count == 0 {
        return Err(format!("edit_file: old_string not found in {}", path.display()));
    }
    if count > 1 {
        return Err(format!(
            "edit_file: old_string matches {count} times in {} — include more context to make it unique",
            path.display()
        ));
    }
    if !auto_approve {
        let approved = prompt_approval(&format!("edit_file: 1 replacement in {path:?}"));
        if !approved {
            return Err("edit_file: denied by user".to_string());
        }
    }
    std::fs::write(&path, content.replacen(&old, &new, 1))
        .map_err(|e| format!("edit_file: write {}: {e}", path.display()))?;
    Ok(format!("edited {} (1 replacement)", path.display()))
}

/// Run `git -C <cwd> <args>` and return stdout (capped). Read-only git helpers.
fn run_git(args: &[&str], cwd: &Path) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e} (is git installed?)"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    if s.trim().is_empty() {
        s = "(no output)".to_string();
    }
    const CAP: usize = 20_000;
    if s.len() > CAP {
        s.truncate(CAP);
        s.push_str("\n…(truncated)");
    }
    Ok(s)
}

fn tool_git(args: &[&str], cwd: &Path) -> Result<String, String> {
    run_git(args, cwd)
}

fn tool_git_diff(input: &Value, cwd: &Path) -> Result<String, String> {
    match input.get("path").and_then(|v| v.as_str()) {
        Some(p) => {
            resolve_path(cwd, p)?; // confine to workspace
            run_git(&["diff", "--", p], cwd)
        }
        None => run_git(&["diff"], cwd),
    }
}

fn tool_git_show(input: &Value, cwd: &Path) -> Result<String, String> {
    let rev = input.get("rev").and_then(|v| v.as_str()).unwrap_or("HEAD");
    if rev.is_empty() || !rev.chars().all(|c| c.is_ascii_alphanumeric() || "._-/~^".contains(c)) {
        return Err("git_show: invalid rev (allowed: alphanumerics and . _ - / ~ ^)".to_string());
    }
    run_git(&["show", "--stat", rev], cwd)
}

fn tool_git_blame(input: &Value, cwd: &Path) -> Result<String, String> {
    let path = require_string(input, "path")?;
    resolve_path(cwd, &path)?; // confine to workspace
    run_git(&["blame", "--", &path], cwd)
}

/// Read-only tools — the set exposed under the `read-only` MCP profile.
pub fn is_read_only(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "read_file"
            | "list_dir"
            | "grep"
            | "git_status"
            | "git_diff"
            | "git_log"
            | "git_show"
            | "git_blame"
            | "list_skills"
            | "read_skill"
    )
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
        let result = execute(&call, &dir, true, PermissionMode::Dangerous);

        assert!(result.ok, "read_file should succeed: {:?}", result.content);
        assert!(result.content.contains("hello, world"));
    }

    #[test]
    fn read_file_missing() {
        let dir = tmpdir();
        let call = make_call("c1", "read_file", json!({"path": "no_such_file.txt"}));
        let result = execute(&call, &dir, true, PermissionMode::Dangerous);
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
        let result = execute(&write_call, &dir, true, PermissionMode::Dangerous);
        assert!(result.ok, "write_file should succeed: {:?}", result.content);

        let read_call = make_call("c2", "read_file", json!({"path": "output.txt"}));
        let read_result = execute(&read_call, &dir, true, PermissionMode::Dangerous);
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
        let result = execute(&call, &dir, true, PermissionMode::Dangerous);
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
        let result = execute(&call, &dir, true, PermissionMode::Dangerous);
        assert!(result.ok, "grep should succeed: {:?}", result.content);
        assert!(result.content.contains("hello"));
        assert!(!result.content.contains("world"), "should not match 'world'");
    }

    #[test]
    fn grep_no_matches() {
        let dir = tmpdir();
        fs::write(dir.join("empty.rs"), "fn foo() {}").unwrap();

        let call = make_call("c1", "grep", json!({"pattern": "XYZZY_NOT_FOUND", "path": "."}));
        let result = execute(&call, &dir, true, PermissionMode::Dangerous);
        assert!(result.ok);
        assert!(result.content.contains("no matches"));
    }

    #[test]
    fn grep_invalid_regex_returns_error() {
        let dir = tmpdir();
        let call = make_call("c1", "grep", json!({"pattern": "[invalid"}));
        let result = execute(&call, &dir, true, PermissionMode::Dangerous);
        assert!(!result.ok);
        assert!(result.content.contains("invalid regex"));
    }

    // --- bash ---

    #[test]
    fn bash_runs_command_auto_approved() {
        let dir = tmpdir();
        let call = make_call("c1", "bash", json!({"command": "echo 'hello from bash'"}));
        let result = execute(&call, &dir, true, PermissionMode::Dangerous);
        assert!(result.ok, "bash should succeed: {:?}", result.content);
        assert!(result.content.contains("hello from bash"));
    }

    #[test]
    fn bash_captures_exit_code_on_failure() {
        let dir = tmpdir();
        let call = make_call("c1", "bash", json!({"command": "exit 42"}));
        let result = execute(&call, &dir, true, PermissionMode::Dangerous);
        // ok can be true (we ran it) but the content should mention the exit code.
        assert!(result.content.contains("42"), "should report non-zero exit code");
    }

    // --- persistent shell (terminal mode) ---

    fn shell_cfg(name: &str, timeout: u64) -> ShellConfig {
        let dir = std::env::temp_dir()
            .join(format!("chatgpt-use-shell-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        ShellConfig { state_dir: dir, timeout_secs: timeout }
    }

    #[test]
    fn persistent_shell_keeps_cwd_between_calls() {
        let cfg = shell_cfg("cwd", 30);
        let def = std::env::temp_dir();
        // First command changes directory…
        let r1 = run_persistent("cd /tmp && echo step1", &def, PermissionMode::Trusted, &cfg).unwrap();
        assert!(r1.contains("step1"), "r1: {r1}");
        // …and the next command should already be there.
        let r2 = run_persistent("pwd", &def, PermissionMode::Trusted, &cfg).unwrap();
        assert!(r2.contains("tmp"), "cwd should persist to /tmp, got: {r2}");
    }

    #[test]
    fn persistent_shell_keeps_exported_env() {
        let cfg = shell_cfg("env", 30);
        let def = std::env::temp_dir();
        run_persistent("export GREETING=hi_there_42", &def, PermissionMode::Trusted, &cfg).unwrap();
        let r = run_persistent("echo $GREETING", &def, PermissionMode::Trusted, &cfg).unwrap();
        assert!(r.contains("hi_there_42"), "exported env should persist, got: {r}");
    }

    #[test]
    fn persistent_shell_times_out_a_hung_command() {
        let cfg = shell_cfg("timeout", 1);
        let def = std::env::temp_dir();
        let start = Instant::now();
        let r = run_persistent("sleep 10 && echo done", &def, PermissionMode::Trusted, &cfg).unwrap();
        assert!(start.elapsed() < Duration::from_secs(6), "should be killed near the 1s timeout");
        assert!(r.contains("timed out"), "should report a timeout, got: {r}");
        assert!(!r.contains("done"), "the command should not have completed");
    }

    // --- skill discovery ---

    #[test]
    fn parse_skill_frontmatter_extracts_name_and_desc() {
        let md = "---\nname: chrome-use\ndescription: Browser automation CLI.\n---\n\n# Goal\nbody";
        let (n, d) = parse_skill_frontmatter(md, "fallback");
        assert_eq!(n, "chrome-use");
        assert_eq!(d, "Browser automation CLI.");
        // No frontmatter → fallback name, empty desc.
        let (n2, d2) = parse_skill_frontmatter("# just a heading", "myskill");
        assert_eq!(n2, "myskill");
        assert_eq!(d2, "");
    }

    #[test]
    fn list_and_read_skill_roundtrip() {
        // Build a fake skills dir and point the discovery at it.
        let root = std::env::temp_dir().join(format!("cgu-skills-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let s = root.join("demo-skill");
        fs::create_dir_all(&s).unwrap();
        fs::write(s.join("SKILL.md"), "---\nname: demo-skill\ndescription: A demo.\n---\n\nRun `demo --go`.").unwrap();
        fs::write(s.join("run.sh"), "echo hi").unwrap();
        let _ = SKILLS_DIR.set(root.clone());

        let listed = tool_list_skills().unwrap();
        assert!(listed.contains("demo-skill"), "list: {listed}");
        assert!(listed.contains("A demo."), "list desc: {listed}");

        let read = tool_read_skill(&json!({"name": "demo-skill"})).unwrap();
        assert!(read.contains("Run `demo --go`."), "read body: {read}");
        assert!(read.contains("run.sh"), "read should list files: {read}");

        // Path-escape guard.
        assert!(tool_read_skill(&json!({"name": "../etc"})).is_err());
        // Missing skill.
        assert!(tool_read_skill(&json!({"name": "nope"})).is_err());
    }

    // --- unknown tool ---

    #[test]
    fn unknown_tool_returns_error() {
        let dir = tmpdir();
        let call = make_call("c1", "no_such_tool", json!({}));
        let result = execute(&call, &dir, true, PermissionMode::Dangerous);
        assert!(!result.ok);
        assert!(result.content.contains("unknown tool"));
    }

    // --- permission gating + sandbox ---

    #[test]
    fn permission_modes_gate_bash() {
        assert!(gate_command("curl http://x", PermissionMode::Safe).is_some());
        assert!(gate_command("echo hi", PermissionMode::Safe).is_none());
        assert!(gate_command("rm -rf /", PermissionMode::Trusted).is_some());
        assert!(gate_command("curl http://x", PermissionMode::Trusted).is_none());
        assert!(gate_command("rm -rf /", PermissionMode::Dangerous).is_none());
        assert!(is_secret_env("OPENAI_API_KEY"));
        assert!(is_secret_env("MY_SECRET"));
        assert!(!is_secret_env("PATH"));
    }

    #[test]
    fn resolve_path_rejects_escapes() {
        let dir = tmpdir();
        assert!(resolve_path(&dir, "../etc/passwd").is_err());
        assert!(resolve_path(&dir, "/etc/passwd").is_err());
        assert!(resolve_path(&dir, "sub/ok.txt").is_ok());
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
