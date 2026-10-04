---
name: chatgpt-use
description: Turn the user's ChatGPT web subscription (Plus/Pro) into a coding-agent backend — no API key, no Codex billing. Use when the user wants to delegate a question, plan, review, debug pass, or whole task to ChatGPT ("ask ChatGPT", "让 ChatGPT 看看/做", "second opinion from GPT"), route work through their paid web subscription instead of API tokens, expose local files/terminal to ChatGPT via its MCP connector, or run ChatGPT as a drop-in Anthropic-compatible model endpoint. Built on chrome-use; requires a Chrome profile logged in to chatgpt.com.
allowed-tools: Bash(chatgpt-use:*), Bash(chrome-use:*)
---

# chatgpt-use

Drives the user's **logged-in ChatGPT web conversation** through `chrome-use` and wires it into the
local machine. Work runs on the flat subscription the user already pays for — not the API, not the
Codex-usage bucket.

**Install / self-heal:** if `chatgpt-use` is missing, install from the GitHub Release, then retry:

```sh
curl -fsSL https://raw.githubusercontent.com/leeguooooo/chatgpt-use/main/install.sh | sh
```

Requires `chrome-use` on PATH (`curl -fsSL https://raw.githubusercontent.com/leeguooooo/chrome-use/main/install.sh | sh`)
and a Chrome profile logged in to chatgpt.com.

## Pick the mode

```sh
# Sidekick — one round trip, caller stays the brain
chatgpt-use ask "<question>" [--file <path> ...] --no-stdin
git diff | chatgpt-use ask "Explain what changed and what might break"

# Structured delegation — plan/review/debug/research verdict packet, optionally --model pro
chatgpt-use ask "<task>" --mode plan|review|debug|research --file <ctx> [--json] [--model pro] --no-stdin
chatgpt-use handoff plan.json --to codex|claude-code [--execute]   # dry-run without --execute

# Closed loop — ChatGPT DOES the task on the project via its MCP connector and reports back
chatgpt-use work "<task>" [--loop] [--max-turns N]     # needs connector + mcp --profile full running

# MCP channel — give ChatGPT native tools on this machine
chatgpt-use mcp --port 8788 --cwd <project>            # read-only by default, safe to tunnel
#   --profile full --permission-mode trusted|dangerous  # write/bash — trusted/local only

# Machine use — one JSON envelope + exit code; stdin is context when piped
chatgpt-use ask --json --no-stdin [--request-id <id>] "<question>"   # then status|resume|cancel <id>
# Piped stdin is context. Not piping anything? Pass --no-stdin: an open but silent stdin
# (some harnesses, or a `while read` loop that ask would drain) fails after 5 s otherwise.

# Any MCP client — ask / status / resume / cancel as tools, same envelope
chatgpt-use agent-mcp      # e.g. claude mcp add chatgpt-use -- chatgpt-use agent-mcp

# Experimental: local agent loop (run) / Anthropic-compatible endpoint (serve)
chatgpt-use run "<task>" [--approve]
chatgpt-use serve --port 8787    # then ANTHROPIC_BASE_URL=http://127.0.0.1:8787 claude
```

One-time setup: `chatgpt-use init` (writes `~/.chatgpt-use/auth.json`). Shared flags:
`--model instant|medium|high|"extra high"|pro` (pro is browser-only), `--profile auto|relay|"Profile N"`,
`--session <tab-group>`, `--project <ChatGPT Project>`.

## Agent etiquette

- `mcp --profile full --permission-mode dangerous` hands ChatGPT a real terminal — only on the
  user's explicit request, never tunnel it, keep the token secret.
- The web surface rate-limits; on a rate-limit dialog, back off rather than hammering.
- `chatgpt-use --help` / `<subcommand> --help` for the full surface.

## Upgrade

When any `chatgpt-use` command prints `chatgpt-use X is available`, tell the user and offer to run
`chatgpt-use upgrade` (it updates the CLI and this skill). Check without changing anything:
`chatgpt-use upgrade --check` (or `--json`). The user may also just say "升级 chatgpt-use" / "upgrade chatgpt-use".

If the skill came from somewhere `upgrade` can't refresh:
- Claude Code plugin: `claude plugin update chatgpt-use@leeguooooo-plugins`
- Whole family: `curl -fsSL https://raw.githubusercontent.com/leeguooooo/plugins/main/upgrade-use-family.sh | sh`
