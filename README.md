# chatgpt-use

> Turn your **ChatGPT web subscription** into a coding-agent backend — **no API key, no Codex billing.**
> Built on [`chrome-use`](https://github.com/leeguooooo/chrome-use), same lineage as [`chatgpt-imagegen`](https://github.com/leeguooooo/chatgpt-imagegen) and [`cookie-use`](https://github.com/leeguooooo/cookie-use).

<p align="center"><em>🚧 Experimental · design phase. The architecture below is the contract; the binary is being built against it.</em></p>

![chatgpt-use](assets/hero.png)

Your Plus / Pro plan already includes a chat surface you've paid for. `chatgpt-use` drives that
**logged-in web conversation** through `chrome-use` — exactly the way `chatgpt-imagegen` drives image
generation — and wires it into your local machine. The result: a coding agent (Claude Code, Codex,
anything) can hand work to ChatGPT, and ChatGPT can read and edit your project files.

The web chat surface is **not the API** and **not the Codex-usage bucket**. So the work runs on quota
you've *already bought* — that's the whole point.

---

## Why this exists

| | API / `codex exec` | **chatgpt-use (web)** |
|---|---|---|
| Auth | `OPENAI_API_KEY` or Codex login | your normal browser login |
| Billing | per-token API spend / Codex-usage limit | **your flat monthly subscription** |
| File access | you build context plumbing | **`read_file` tool — no tunnel** |
| Setup | keys, env, gateways | `chrome-use` + a logged-in tab |

If you're already paying for ChatGPT Plus/Pro and *also* burning API credits or Codex-usage limits
from your coding agent, this closes the gap: route the cheap-and-already-paid work to the browser.

---

## Three modes

`chatgpt-use` is one engine — a `chrome-use`-driven **channel** to the ChatGPT web conversation (send a
message, wait for the reply, parse it) — exposed three ways depending on **who's the brain**.

### Mode 1 · 副手 / Sidekick — `chatgpt-use ask`

![sidekick](assets/mode1-sidekick.png)

**Your harness stays the brain.** Claude Code / Codex keeps planning and calling its own tools, and
delegates a single sub-task — reasoning, code generation, a review pass — to ChatGPT when it wants a
second brain. One round trip, no tool loop.

```bash
# one-shot: pipe context in, get an answer back
chatgpt-use ask "Review this diff for race conditions" --file src/server.rs

# or feed it whatever you already gathered
git diff | chatgpt-use ask "Explain what changed and what might break"
```

- The **caller** decides what context to send — `chatgpt-use` just relays it and returns ChatGPT's text.
- ChatGPT does **not** touch your machine in this mode.
- Borrows the web-driving practices proven in
  [`chatgpt-imagegen`](https://github.com/leeguooooo/chatgpt-imagegen): profile auto-detection
  (`relay` → logged-in profile), composer polling, rate-limit-dialog detection, in-page
  authenticated `fetch`, and conversation filing under a ChatGPT **Project**.

### Mode 2 · 大脑 / Brain — `chatgpt-use run`

![brain](assets/mode2-brain.png)

**ChatGPT is the brain; your machine is the hands.** A local agent loop — the same shape as Codex or
Claude Code — but the model is your web subscription:

1. `chatgpt-use` seeds the conversation with a **system prompt that defines a tool protocol**.
2. ChatGPT replies with a structured **tool call** (a fenced JSON block — see the caveat below).
3. The local harness **executes that tool** (`read_file`, `write_file`, `bash`, `grep`, `list_dir`, …)
   and feeds the observation back into the chat.
4. Loop until ChatGPT declares the task done.

```bash
chatgpt-use run "Add a --json flag to the status command and update the tests"
```

**This is why goal "let ChatGPT read my files" needs no tunnel and no exposed file server.** File
access *is* the `read_file` / `grep` tools: the local harness reads the bytes and hands them into the
conversation. ChatGPT never reaches back to your machine — it just asks, and the hands obey.

### Mode 3 · 替身 / Drop-in model — `chatgpt-use serve`

![drop-in](assets/mode3-dropin.png)

**Claude Code stays exactly as it is — its agent loop, its tools, its UX — but the model behind it is
secretly ChatGPT.** `chatgpt-use serve` exposes a local **Anthropic-compatible endpoint**
(`/v1/messages`, streaming). Point Claude Code at it:

```bash
chatgpt-use serve --port 8787 &
ANTHROPIC_BASE_URL=http://127.0.0.1:8787 ANTHROPIC_AUTH_TOKEN=whatever claude
```

Now every model call Claude Code makes — the thing that *spends Anthropic model tokens* — is
intercepted, translated into a prompt, driven through your ChatGPT web subscription, and translated
back into Anthropic's response shape (**including `tool_use` blocks**, so Claude Code's own tools keep
working). Claude Code never knows its brain was swapped.

- **No model tokens.** Claude Code's loop runs locally and free; the tokens it would have billed are
  served by your flat subscription instead.
- Reuses Mode 2's **text tool-call protocol + parser** — but instead of running our own loop, it
  re-encodes ChatGPT's tool calls as Anthropic `tool_use` blocks and hands them back to Claude Code,
  which runs its own tools.
- The most ambitious and most fragile mode (see caveats): Claude Code's prompts are large, tool-call
  fidelity over a text protocol is imperfect, and the web surface rate-limits. **Experimental².**

All three modes share one engine: **Mode 1** is Mode 2 with tools off; **Mode 3** is Mode 2's tool
protocol re-dressed as an Anthropic API so an *existing* harness can wear ChatGPT as its model.

---

## How it works

```
  ┌─────────────────────────────────────────────────────────────────┐
  │  chatgpt-use  (Rust CLI)                                         │
  │                                                                  │
  │   task ─▶ system prompt + tool protocol                          │
  │              │                                                   │
  │              ▼                                                   │
  │      ┌───────────────┐   eval/send/poll   ┌──────────────────┐   │
  │      │  agent loop   │ ─────────────────▶ │  chrome-use      │   │
  │      │  + tool exec  │ ◀───────────────── │  (logged-in tab) │   │
  │      └───────────────┘   parsed reply     └────────┬─────────┘   │
  │              │                                     │             │
  │     read_file/write_file/bash/grep         ChatGPT web chat      │
  │              ▼                              (your subscription)  │
  │        your project files                                       │
  └─────────────────────────────────────────────────────────────────┘
```

Everything page-side goes through `chrome-use eval <js>` (run JS in the page, get JSON back). Sending
a prompt = fill `#prompt-textarea` + click send. "Reply done" = poll until the stop/streaming control
disappears, watching for the *"Too many requests"* dialog. All proven in `chatgpt-imagegen`.

---

## The honest caveats

This is a clever hack on a surface that was never meant to be an API. We're upfront about it:

- **No native function-calling.** The web chat has no tool-call API (that's API-only). Tool calls are a
  **text protocol** we define in the system prompt and parse out of the reply. This is the make-or-break
  bet — and live testing **confirmed it's the real wall**: current web models are strongly grounded and
  often refuse to role-play tool execution from an instruction message alone ("I can't run those tools —
  please paste the file"). Overcoming this needs **conversation priming** (seeding a real-looking prior
  tool round-trip as actual turns, not just described), which is the active hardening work for Modes 2/3.
  Mode 1 (no tools) is unaffected and works today.
- **Rate limits are real.** Driving the one shared logged-in tab, the page rate-limits aggressively, so
  the channel runs at **concurrency 1** and queues across processes (flock), same as `chatgpt-imagegen`.
- **It's slower than the API.** You're waiting on a browser rendering a chat. Fine for offloading;
  not for tight latency loops.
- **Mode 3 is the deep end.** A full chat harness's traffic squeezed through a browser chat box: slow,
  occasionally wrong, and only as good as the tool-call translation. It's a proof-of-concept of "free
  Claude Code", not a daily driver — yet.
- **ToS:** you are driving *your own* logged-in browser session. Use it within your plan's terms.
- **macOS first** (matches `chrome-use` / `cookie-use`); other platforms follow `chrome-use`.

---

## Install

> Distribution follows the GitHub-Release route (no npm, no token). Once the first binary ships:

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/chatgpt-use/main/install.sh | sh
```

`chatgpt-use` **requires `chrome-use`** on `PATH`. If it's missing:

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/chrome-use/main/install.sh | sh
```

Then make sure you have a Chrome profile logged in to chatgpt.com (or connect your live Chrome via
`chrome-use extension connect`).

---

## Usage cheatsheet

```bash
# Mode 1 — sidekick (harness is the brain)
chatgpt-use ask "<question>" [--file <path> ...] [--profile auto|relay|"Profile N"]

# Mode 2 — brain (ChatGPT drives local tools in a loop)
chatgpt-use run "<task>" [--cwd <dir>] [--approve] [--max-steps N]

# Mode 3 — drop-in model (Claude Code keeps its loop; ChatGPT is the model)
chatgpt-use serve --port 8787
#   then: ANTHROPIC_BASE_URL=http://127.0.0.1:8787 ANTHROPIC_AUTH_TOKEN=x claude

# shared flags (mirroring chatgpt-imagegen)
#   --profile   auto (default) | relay | "Profile 3"
#   --session   reuse a chrome-use tab group across runs
#   --project   file the conversation under a ChatGPT Project
```

*(Flags are the design target; check `chatgpt-use --help` once built for the authoritative set.)*

---

## Roadmap

- [x] Design: three modes on one `chrome-use`-driven channel
- [x] Channel core (send / poll / parse) — ported from `chatgpt-imagegen`, **live-verified**
- [x] Tool protocol + parser + executor (`read_file`, `write_file`, `bash`, `grep`, `list_dir`) — unit-tested
- [x] Mode 1 `ask` (one-shot, no tools) — **live-verified end-to-end**
- [x] Mode 2 `run` agent loop + approval gate — implemented (see fidelity note ⬇)
- [x] Mode 3 `serve` — Anthropic-compatible `/v1/messages` shim → Claude Code drop-in — implemented PoC
- [x] `install.sh` + GitHub-Release workflow
- [ ] **Tool-fidelity hardening (the open problem):** conversation priming so web models reliably
  emit tool calls instead of refusing — gates Modes 2 & 3 from "implemented" to "reliable"
- [ ] Optional: a UI shell over the loop (TUI / menubar) for live progress & tool approval

> **Status (honest):** Mode 1 works live. Modes 2 & 3 are fully built, compile clean, and pass unit
> tests, but live runs hit the documented wall — ChatGPT web refuses to role-play tools from a single
> prompt. The engine is done; the remaining work is prompt/priming, not plumbing.

---

## Credits

Stands on the shoulders of [`chrome-use`](https://github.com/leeguooooo/chrome-use) (browser
automation), [`chatgpt-imagegen`](https://github.com/leeguooooo/chatgpt-imagegen) (the web-driving
playbook), and [`cookie-use`](https://github.com/leeguooooo/cookie-use) (the CLI-on-chrome-use model).

Idea seeded by [@VincentLogic](https://x.com/VincentLogic/status/2066800292604026943).

## License

MIT
