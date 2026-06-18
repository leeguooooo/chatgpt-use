//! The shared engine: a `chrome-use`-driven channel to a logged-in ChatGPT web
//! conversation. Every mode goes through here. Port the proven web-driving
//! practices from chatgpt-imagegen (read its source at
//! /Users/leo/github.com/chatgpt-imagegen/chatgpt-imagegen):
//!   - locate the `chrome-use` binary; pick the browser (relay first, then a
//!     logged-in profile; honor `profile = auto|relay|"Profile N"`)
//!   - open chatgpt.com (optionally inside a ChatGPT Project), wait for the
//!     #prompt-textarea composer
//!   - submit a message; poll page state until the stop/streaming control
//!     disappears (reply complete); detect the "Too many requests" dialog
//!   - read the newest assistant message text/markdown back out
//! All page interaction goes through `chrome-use eval <js>` returning JSON.
//!
//! Concurrency is 1 (it drives the one shared logged-in tab and the page rate-
//! limits hard) — serialize across processes like chatgpt-imagegen does.
//!
//! Owned by the CORE agent.

use anyhow::{anyhow, bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

// Accepted chrome-use binary names, newest name first (mirrors chatgpt-imagegen).
const AB_BIN_CANDIDATES: &[&str] = &["chrome-use", "agent-browser", "agent-browser-stealth", "abs"];

const WEB_NEW_CHAT_URL: &str = "https://chatgpt.com/";
const WEB_PROJECT_URL_TPL: &str = "https://chatgpt.com/g/{gizmo_id}/project";

const RATE_LIMIT_MSG: &str =
    "chatgpt.com rate-limited this account ('Too many requests') — the page \
     surface needs a few minutes of quiet before it will serve again.";

// JS: poll composer presence + rate-limit dialog (mirrors _JS_COMPOSER in chatgpt-imagegen).
const JS_COMPOSER: &str = r#"(() => {
  const dlg = [...document.querySelectorAll('[role="dialog"]')]
    .map(d => d.textContent || '').join(' ');
  return JSON.stringify({
    composer: !!document.querySelector('#prompt-textarea'),
    limited: /too many requests|requests too quickly/i.test(dlg),
  });
})()"#;

// JS: poll generation/reply state: stop button present? newest assistant text?
// rate-limited? Mirrors _JS_STATE in chatgpt-imagegen but without image scraping.
const JS_STATE: &str = r#"(() => {
  const stop = !!document.querySelector(
    'button[data-testid="stop-button"], button[aria-label*="Stop" i]'
  );
  const a = document.querySelectorAll('[data-message-author-role="assistant"]');
  const lastA = a[a.length - 1];
  const dlg = [...document.querySelectorAll('[role="dialog"]')]
    .map(d => d.textContent || '').join(' ');
  return JSON.stringify({
    stop,
    assistant_count: a.length,
    limited: /too many requests|requests too quickly/i.test(dlg),
    atext: lastA ? (lastA.innerText || lastA.textContent || '').trim() : ""
  });
})()"#;

// JS: scrape the full innerText of the last assistant message.
const JS_LAST_ASSISTANT: &str = r#"(() => {
  const a = document.querySelectorAll('[data-message-author-role="assistant"]');
  const lastA = a[a.length - 1];
  if (!lastA) return JSON.stringify("");
  return JSON.stringify((lastA.innerText || lastA.textContent || "").trim());
})()"#;

// JS: resolve or create a ChatGPT Project by exact display name.
// Returns {ok, id, created, error?}. Mirrors _JS_ENSURE_PROJECT in chatgpt-imagegen.
fn js_ensure_project(name: &str) -> String {
    let name_json = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(async () => {{
  try {{
    const name = {name_json};
    const sess = await fetch('/api/auth/session', {{credentials: 'include'}})
      .then(r => r.json()).catch(() => null);
    if (!sess || !sess.accessToken)
      return JSON.stringify({{ok: false, error: 'no accessToken in /api/auth/session'}});
    const h = {{Authorization: 'Bearer ' + sess.accessToken,
               'Content-Type': 'application/json'}};
    const find = async () => {{
      const r = await fetch(
        '/backend-api/gizmos/snorlax/sidebar?conversations_per_gizmo=0',
        {{credentials: 'include', headers: h}});
      if (!r.ok) throw new Error('project list HTTP ' + r.status);
      for (const it of (await r.json()).items || []) {{
        const g = it.gizmo && it.gizmo.gizmo;
        if (g && g.display && g.display.name === name) return g.id;
      }}
      return null;
    }};
    let id = await find();
    if (id) return JSON.stringify({{ok: true, id, created: false}});
    const mk = await fetch('/backend-api/projects', {{
      method: 'POST', credentials: 'include', headers: h,
      body: JSON.stringify({{name: name, instructions: ''}})}});
    if (!mk.ok) return JSON.stringify({{ok: false, error: 'project create HTTP ' + mk.status}});
    const j = await mk.json().catch(() => null);
    id = j && ((j.gizmo && (j.gizmo.id || (j.gizmo.gizmo && j.gizmo.gizmo.id))) || j.id);
    if (!id) id = await find();
    if (!id) return JSON.stringify({{ok: false, error: 'created but could not resolve id'}});
    return JSON.stringify({{ok: true, id, created: true}});
  }} catch (e) {{ return JSON.stringify({{ok: false, error: String(e)}}); }}
}})()"#,
        name_json = name_json
    )
}

#[derive(Debug, Clone)]
pub struct ChannelOptions {
    /// auto | relay | a Chrome profile name.
    pub profile: String,
    /// chrome-use session name (None → derive a per-pid default).
    pub session: Option<String>,
    /// ChatGPT Project name to file the conversation under ("" → plain chat).
    pub project: String,
    /// Per-turn wall-clock budget in seconds.
    pub timeout_secs: u64,
    /// Browser-channel model to select: pro | thinking | instant | <raw label>.
    /// None → use the account default. (Pro is reachable only via the browser.)
    pub model: Option<String>,
}

/// A live conversation. `send` keeps appending turns to the SAME chat, so
/// ChatGPT retains context across calls — the multi-turn loops (run/serve) only
/// send the new turn, not the whole history.
pub struct Channel {
    /// Resolved path to the chrome-use binary.
    ab: PathBuf,
    /// chrome-use session name.
    session: String,
    /// Per-turn timeout in seconds.
    timeout_secs: u64,
}

impl Channel {
    /// Connect: find chrome-use, choose a logged-in browser, open ChatGPT (in
    /// the project if set), and wait for the composer. Errors clearly if no
    /// logged-in browser is available or the account is rate-limited.
    pub fn connect(opts: &ChannelOptions) -> Result<Self> {
        let ab = find_chrome_use().ok_or_else(|| {
            anyhow!(
                "`chrome-use` is not installed — install it (no npm, no token):\n  \
                 curl -fsSL https://raw.githubusercontent.com/leeguooooo/chrome-use/main/install.sh | sh"
            )
        })?;

        let session = opts
            .session
            .clone()
            .unwrap_or_else(|| format!("chatgpt-use-{}", std::process::id()));

        let timeout_secs = opts.timeout_secs;

        // Build candidate profile list (mirrors chatgpt-imagegen run_web logic).
        // None in the list means "relay" (no --profile flag to chrome-use).
        let profile_lower = opts.profile.trim().to_lowercase();
        let candidates: Vec<Option<String>> = match profile_lower.as_str() {
            "relay" | "off" | "current" => vec![None],
            "auto" => {
                // relay first, then any offline-detected logged-in profiles.
                let mut v: Vec<Option<String>> = vec![None];
                v.extend(detect_logged_in_profiles().into_iter().map(Some));
                v
            }
            _ => vec![Some(opts.profile.trim().to_string())],
        };

        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let mut opened = false;

        for prof in &candidates {
            let label = prof.as_deref().unwrap_or("current Chrome (relay)");
            eprintln!("opening ChatGPT via {label}");

            match try_open(&ab, &session, WEB_NEW_CHAT_URL, prof.as_deref(), deadline) {
                Ok(true) => {
                    eprintln!("using {label}");
                    opened = true;
                    break;
                }
                Ok(false) => {
                    // composer never appeared — try next candidate
                    ab_close(&ab, &session);
                }
                Err(e) => {
                    ab_close(&ab, &session);
                    let msg = e.to_string();
                    if msg.contains("rate-limited") || msg.contains("Too many") {
                        bail!("{}", RATE_LIMIT_MSG);
                    }
                    // non-rate-limit error: log and try next candidate
                    eprintln!("warning: {label} failed: {e}");
                }
            }
        }

        if !opened {
            bail!(
                "no logged-in ChatGPT browser available (tried {} candidate(s)). \
                 Sign in to chatgpt.com in Chrome.",
                candidates.len()
            );
        }

        let chan = Channel { ab, session, timeout_secs };

        // Navigate into a ChatGPT Project FIRST — it loads a new page and would
        // reset any model selection, so model selection must come afterwards.
        let project = opts.project.trim().to_string();
        if !project.is_empty() {
            let proj_deadline = Instant::now() + Duration::from_secs(timeout_secs);
            if let Err(e) = chan.enter_project(&project, proj_deadline) {
                eprintln!("warning: project {project:?} unavailable ({e}); using a plain chat");
                // best-effort restore to plain chat
                let restore_deadline = Instant::now() + Duration::from_secs(30);
                let _ = ab_open(&chan.ab, &chan.session, WEB_NEW_CHAT_URL, None, restore_deadline);
                let _ = wait_composer(&chan.ab, &chan.session, restore_deadline, 15);
            }
        }

        // Select the Intelligence level on the now-settled composer (after any
        // project navigation), best-effort.
        if let Some(ref model) = opts.model {
            let model_deadline = Instant::now() + Duration::from_secs(timeout_secs.min(30));
            if let Err(e) = chan.select_model(model, model_deadline) {
                eprintln!("warning: could not select model {model:?}: {e}; using account default");
            }
        }

        Ok(chan)
    }

    /// Send one message and return ChatGPT's completed reply as text/markdown.
    pub fn send(&mut self, message: &str) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(self.timeout_secs);

        let remaining_secs = || {
            deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::from_secs(2))
                .as_secs_f64()
                .max(2.0)
        };

        // Snapshot the current number of assistant messages so we can detect
        // when a NEW one arrives.
        let baseline_count: u64 = {
            let js = r#"(() => {
              const a = document.querySelectorAll('[data-message-author-role="assistant"]');
              return JSON.stringify(a.length);
            })()"#;
            ab_eval(&self.ab, js, &self.session, remaining_secs())
                .ok()
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        };

        // Click the composer, type the message, press Enter.
        // Using keyboard type (not fill) fires React's input events so the send
        // button stays bound to the live content. Mirrors chatgpt-imagegen.
        ab_cmd(&self.ab, &["click", "#prompt-textarea"], &self.session, remaining_secs())
            .context("clicking #prompt-textarea")?;
        // Type the message in CHARACTER chunks. A single `keyboard type` call with
        // a multi-KB argument overruns chrome-use's IPC and fails with EAGAIN
        // ("Resource temporarily unavailable"); chunking keeps each call small
        // while still firing the real input events React/ProseMirror needs.
        // Split on char boundaries (the prompt contains multibyte chars).
        const TYPE_CHUNK_CHARS: usize = 400;
        let chars: Vec<char> = message.chars().collect();
        if chars.len() <= TYPE_CHUNK_CHARS {
            ab_cmd(&self.ab, &["keyboard", "type", message], &self.session, remaining_secs())
                .context("typing message into composer")?;
        } else {
            for chunk in chars.chunks(TYPE_CHUNK_CHARS) {
                let piece: String = chunk.iter().collect();
                ab_cmd(&self.ab, &["keyboard", "type", &piece], &self.session, remaining_secs())
                    .context("typing message chunk into composer")?;
            }
        }
        ab_cmd(&self.ab, &["press", "Enter"], &self.session, remaining_secs())
            .context("pressing Enter to submit")?;

        // Fallback: if Enter didn't submit (text still in the box), click send button.
        let still_there: bool = ab_eval(
            &self.ab,
            r#"(() => {
              const t = (document.querySelector('#prompt-textarea') || {}).textContent || '';
              return JSON.stringify(t.trim().length > 0);
            })()"#,
            &self.session,
            remaining_secs(),
        )
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

        if still_there {
            let _ = ab_cmd(
                &self.ab,
                &["click", r#"button[data-testid="send-button"]"#],
                &self.session,
                remaining_secs(),
            );
        }

        // Poll until the stop button is gone AND a new assistant message count
        // is larger than baseline.
        let poll_interval = Duration::from_millis(2000);
        let mut last_atext = String::new();
        let mut idle_count = 0u32;
        // After 30 idle polls (~60s) with no progress we give up rather than
        // hanging until the total timeout.
        const IDLE_LIMIT: u32 = 30;

        // Heartbeat: the page can think silently for minutes, so emit an
        // elapsed-time progress line to stderr (~every 5s) so the wait is visible.
        let started = Instant::now();
        let mut last_beat: u64 = 0;
        eprintln!("[   0.0s] submitted; waiting for reply");

        loop {
            if Instant::now() >= deadline {
                bail!(
                    "timed out after {}s waiting for ChatGPT to complete the reply",
                    self.timeout_secs
                );
            }
            std::thread::sleep(poll_interval);

            let st = match ab_eval(&self.ab, JS_STATE, &self.session, remaining_secs()) {
                Ok(v) if v.is_object() => v,
                _ => continue,
            };

            if st.get("limited").and_then(|v| v.as_bool()).unwrap_or(false) {
                bail!(
                    "{} The prompt was already submitted; check the conversation \
                     or retry in a few minutes.",
                    RATE_LIMIT_MSG
                );
            }

            let stop = st.get("stop").and_then(|v| v.as_bool()).unwrap_or(false);
            let cur_count = st
                .get("assistant_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            // Throttled heartbeat (~every 5s).
            let elapsed = started.elapsed().as_secs();
            if elapsed >= last_beat + 5 {
                last_beat = elapsed;
                let phase = if stop { "generating" } else { "waiting for reply" };
                eprintln!("[{elapsed:5}.0s] {phase}");
            }
            let atext = st
                .get("atext")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if !atext.is_empty() {
                last_atext = atext;
            }

            if stop {
                // still streaming — reset idle counter
                idle_count = 0;
                continue;
            }

            if cur_count > baseline_count {
                // A new (non-streaming) assistant turn is present. Done.
                break;
            }

            if cur_count > 0 {
                // Count hasn't changed yet, but there is at least one assistant turn.
                idle_count += 1;
                if idle_count >= IDLE_LIMIT {
                    // Tolerate the case where the count happened not to increment
                    // (same-conversation fast reply); proceed to scrape.
                    break;
                }
            }
        }

        // Scrape the last assistant message — prefer innerText (rendered markdown).
        let reply_text = ab_eval(
            &self.ab,
            JS_LAST_ASSISTANT,
            &self.session,
            remaining_secs(),
        )
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| last_atext.clone());

        if reply_text.trim().is_empty() {
            bail!("scraped an empty reply from ChatGPT");
        }

        Ok(reply_text)
    }

    /// Close the tab (best-effort), matching chatgpt-imagegen's try/finally.
    pub fn close(self) {
        ab_close(&self.ab, &self.session);
    }

    /// Navigate the open session into the named ChatGPT Project, creating it on
    /// first use. Mirrors `_enter_project` in chatgpt-imagegen.
    fn enter_project(&self, name: &str, deadline: Instant) -> Result<()> {
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::from_secs(2))
                .as_secs_f64()
                .max(2.0)
        };

        let js = js_ensure_project(name);
        let res = ab_eval(&self.ab, &js, &self.session, remaining().min(30.0))
            .context("resolving ChatGPT Project")?;

        let ok = res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ok {
            let detail = res
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("could not resolve project: {detail}");
        }

        let gizmo_id = res
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("project resolve returned no id"))?;

        let created = res
            .get("created")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        eprintln!(
            "using project {name:?}{}",
            if created { " (created)" } else { "" }
        );

        let project_url = WEB_PROJECT_URL_TPL.replace("{gizmo_id}", gizmo_id);
        ab_open(&self.ab, &self.session, &project_url, None, deadline)?;

        if !wait_composer(&self.ab, &self.session, deadline, 15)? {
            bail!("project page composer never appeared");
        }

        Ok(())
    }

    /// Select an "Intelligence" level in the ChatGPT composer (verified against
    /// the live UI 2026-06: the composer has one picker button whose text is the
    /// current level — Instant / Medium / High / Extra High / Pro — and opens a
    /// menu of those items). GPT-5.5 *Pro* is the "Pro" level here.
    ///
    /// Two non-obvious facts the live DOM forced:
    ///   1. The picker opens ONLY on a real input click — a JS `element.click()`
    ///      does nothing — so we fetch its coords and click via `chrome-use click
    ///      x y` (a CDP input event).
    ///   2. The menu ITEMS, by contrast, respond fine to JS `element.click()`.
    ///
    /// Best-effort: warns and continues with the account default if anything
    /// can't be found. Labels are normalised case-insensitively:
    ///   pro → "Pro", instant → "Instant", medium → "Medium", high → "High",
    ///   "extra high" → "Extra High"; anything else passes through verbatim.
    fn select_model(&self, model: &str, deadline: Instant) -> Result<()> {
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::from_secs(2))
                .as_secs_f64()
                .max(2.0)
        };

        let model_lower = model.trim().to_lowercase();
        let target_label: &str = match model_lower.as_str() {
            "pro" => "Pro",
            "instant" => "Instant",
            "medium" => "Medium",
            "high" => "High",
            "extra high" | "extrahigh" | "extra-high" => "Extra High",
            _ => model.trim(), // raw label passed through unchanged
        };
        let target_json = serde_json::to_string(target_label)
            .unwrap_or_else(|_| format!("\"{}\"", target_label));

        // Step 1: locate the composer picker button and return its CENTER coords.
        // The picker is a button whose ENTIRE text is the current level label
        // (Instant / Medium / High / Extra High / Pro). Search document-wide (the
        // project page nests the composer deeper than a fixed parent-walk reaches)
        // and exclude the account/profile button ("ChatGPT Pro …"). The picker can
        // render a beat after #prompt-textarea, so the caller retries.
        let js_find_picker = r#"(() => {
  const LEVELS = ['instant','medium','high','extra high','pro'];
  const isProfile = el => {
    const al = (el.getAttribute('aria-label') || '').toLowerCase();
    return al.includes('profile') || al.includes('account');
  };
  for (const b of document.querySelectorAll('button')) {
    if (isProfile(b)) continue;
    const txt = (b.textContent || '').trim();
    if (txt.length === 0 || txt.length > 16) continue;
    if (!LEVELS.includes(txt.toLowerCase())) continue;
    const r = b.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) continue;        // not visible
    if (r.top < 0 || r.left < 0) continue;
    return JSON.stringify({ok: true, x: Math.round(r.left + r.width/2),
                           y: Math.round(r.top + r.height/2), label: txt});
  }
  return JSON.stringify({ok: false, error: 'composer level picker not found'});
})()"#;

        // Retry: the picker can appear shortly after the composer.
        let mut pick = serde_json::Value::Null;
        for attempt in 0..6 {
            pick = ab_eval(&self.ab, js_find_picker, &self.session, remaining())?;
            if pick.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                break;
            }
            if attempt < 5 {
                std::thread::sleep(Duration::from_millis(600));
            }
        }
        if !pick.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let detail = pick.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
            bail!("model picker button not found: {detail}");
        }
        let (x, y) = match (pick.get("x").and_then(|v| v.as_i64()), pick.get("y").and_then(|v| v.as_i64())) {
            (Some(x), Some(y)) => (x, y),
            _ => bail!("model picker coords missing"),
        };

        // Real CDP click to open the menu (JS .click() does NOT open it).
        let (xs, ys) = (x.to_string(), y.to_string());
        ab_cmd(&self.ab, &["click", &xs, &ys], &self.session, remaining())
            .context("clicking the composer level picker")?;
        std::thread::sleep(Duration::from_millis(450));

        // Step 2: click the menu item whose text whole-word matches the target.
        // Menu items DO respond to JS element.click().
        let js_select_item = format!(
            r#"(() => {{
  const target = {target_json};
  const norm = s => (s || '').toLowerCase();
  const wordEq = (txt, t) => {{
    const tt = norm(t);
    if (norm(txt) === tt) return true;
    return norm(txt).split(/[^a-z0-9.]+/).filter(Boolean).includes(tt);
  }};
  const items = document.querySelectorAll('[role="menuitem"], [role="menuitemradio"], [role="option"]');
  for (const el of items) {{
    const txt = (el.textContent || '').trim();
    if (wordEq(txt, target)) {{ el.click(); return JSON.stringify({{ok: true, matched: txt}}); }}
  }}
  return JSON.stringify({{ok: false, error: 'menu item not found for: ' + target}});
}})()"#,
            target_json = target_json
        );

        let select_result = ab_eval(&self.ab, &js_select_item, &self.session, remaining())?;
        if !select_result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let detail = select_result.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
            bail!("model menu item not found: {detail}");
        }
        std::thread::sleep(Duration::from_millis(350));

        // Step 3: verify the picker label now reflects the target (whole-word).
        let verify_json = target_json.clone();
        let js_verify = format!(
            r#"(() => {{
  const target = {verify_json};
  const ta = document.querySelector('#prompt-textarea');
  let area = ta;
  for (let i = 0; i < 6 && area && area.parentElement; i++) area = area.parentElement;
  area = area || document;
  const norm = s => (s || '').toLowerCase();
  for (const b of area.querySelectorAll('button')) {{
    const txt = (b.textContent || '').trim();
    if (txt.length > 0 && txt.length <= 16 && norm(txt) === norm(target)) {{
      return JSON.stringify({{ok: true, label: txt}});
    }}
  }}
  return JSON.stringify({{ok: false}});
}})()"#,
            verify_json = verify_json
        );
        let verified = ab_eval(&self.ab, &js_verify, &self.session, remaining())
            .ok()
            .and_then(|v| v.get("ok").and_then(|b| b.as_bool()))
            .unwrap_or(false);
        if verified {
            eprintln!("model selected: {target_label:?}");
        } else {
            eprintln!("warning: clicked {target_label:?} but could not confirm the picker switched");
        }

        Ok(())
    }
}

// ---- chrome-use helpers (mirrors _ab / _ab_eval in chatgpt-imagegen) --------

/// Locate the chrome-use binary: search PATH, then ~/.local/bin.
fn find_chrome_use() -> Option<PathBuf> {
    for name in AB_BIN_CANDIDATES {
        if let Some(p) = which_bin(name) {
            return Some(p);
        }
    }
    // Also check ~/.local/bin — common for manual installs on macOS/Linux.
    if let Some(home) = std::env::var_os("HOME") {
        let local_bin = PathBuf::from(home).join(".local").join("bin");
        for name in AB_BIN_CANDIDATES {
            let candidate = local_bin.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Minimal `which`-equivalent: search PATH for a binary name.
fn which_bin(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let p = PathBuf::from(dir).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Run a chrome-use subcommand with optional profile; return stdout.
/// Mirrors `_ab` in chatgpt-imagegen: `--profile <p>` precedes the subcommand;
/// `--session <s>` trails everything.
fn ab_cmd_with_profile(
    ab: &PathBuf,
    args: &[&str],
    session: &str,
    profile: Option<&str>,
    _timeout_secs: f64,
) -> Result<String> {
    let mut cmd = Command::new(ab);
    if let Some(prof) = profile {
        cmd.args(["--profile", prof]);
    }
    cmd.args(args);
    cmd.args(["--session", session]);

    let output = cmd
        .output()
        .with_context(|| format!("failed to run chrome-use {args:?}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let tail: String = stderr
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" / ");
        let detail = if tail.is_empty() {
            stdout.trim().to_string()
        } else {
            tail
        };
        bail!(
            "chrome-use {:?} failed (exit {}): {}",
            args,
            output.status.code().unwrap_or(-1),
            &detail[..detail.len().min(300)]
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a chrome-use subcommand (no profile override).
fn ab_cmd(ab: &PathBuf, args: &[&str], session: &str, timeout_secs: f64) -> Result<String> {
    ab_cmd_with_profile(ab, args, session, None, timeout_secs)
}

/// Run JS in the page and double-decode the returned JSON string.
///
/// Convention (mirrors `_ab_eval`): the JS does `return JSON.stringify(value)`;
/// chrome-use prints THAT string JSON-encoded, so we decode twice — once to get
/// the inner JSON text, once to parse it into a value.
fn ab_eval(
    ab: &PathBuf,
    js: &str,
    session: &str,
    timeout_secs: f64,
) -> Result<serde_json::Value> {
    let raw = ab_cmd(ab, &["eval", js], session, timeout_secs)?;

    // Scan from the last non-empty line for the first that decodes to a string.
    for line in raw.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // First decode: chrome-use wraps the page's return in a JSON string.
        if let Ok(inner) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(s) = inner.as_str() {
                // Second decode: the page did JSON.stringify(value).
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
                    return Ok(val);
                }
                // Not JSON-parseable — return as a plain string value.
                return Ok(serde_json::Value::String(s.to_string()));
            }
            // Not a string wrapper — return as-is.
            return Ok(inner);
        }
    }

    bail!(
        "could not parse chrome-use eval output: {:?}",
        &raw[..raw.len().min(200)]
    )
}

/// Open a URL in the session's tab (optionally with a Chrome profile).
fn ab_open(
    ab: &PathBuf,
    session: &str,
    url: &str,
    profile: Option<&str>,
    deadline: Instant,
) -> Result<()> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(2))
        .as_secs_f64()
        .min(30.0)
        .max(5.0);
    ab_cmd_with_profile(ab, &["open", url], session, profile, remaining)?;
    Ok(())
}

/// Close the session's tab — best effort, never raises.
fn ab_close(ab: &PathBuf, session: &str) {
    let _ = ab_cmd(ab, &["close"], session, 15.0);
}

/// Open a new chat tab and wait for the composer. Returns Ok(true) if ready.
fn try_open(
    ab: &PathBuf,
    session: &str,
    url: &str,
    profile: Option<&str>,
    deadline: Instant,
) -> Result<bool> {
    ab_open(ab, session, url, profile, deadline)?;
    wait_composer(ab, session, deadline, 15)
}

/// Poll until `#prompt-textarea` is on the page (mirrors `_wait_composer`).
/// Returns `Ok(true)` when the composer is ready, `Ok(false)` on timeout.
/// Bails with an error if the rate-limit dialog is detected.
fn wait_composer(
    ab: &PathBuf,
    session: &str,
    deadline: Instant,
    tries: u32,
) -> Result<bool> {
    for _ in 0..tries {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::from_secs(1))
            .as_secs_f64()
            .min(20.0)
            .max(2.0);

        match ab_eval(ab, JS_COMPOSER, session, remaining) {
            Ok(st) if st.is_object() => {
                if st.get("limited").and_then(|v| v.as_bool()).unwrap_or(false) {
                    bail!("{}", RATE_LIMIT_MSG);
                }
                if st.get("composer").and_then(|v| v.as_bool()).unwrap_or(false) {
                    return Ok(true);
                }
            }
            _ => {}
        }

        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(false)
}

/// Detect Chrome profiles that have an active chatgpt.com session cookie.
/// Best-effort; returns an empty Vec rather than erroring (relay path still works).
/// The Python reference reads the Cookies SQLite DB; we skip that here to avoid
/// adding a sqlite3 dep — callers can pass --profile explicitly when needed.
fn detect_logged_in_profiles() -> Vec<String> {
    Vec::new()
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_ensure_project_embeds_name() {
        let js = js_ensure_project("my-project");
        assert!(js.contains("my-project"), "JS should embed the project name");
        assert!(js.contains("backend-api/projects"), "JS should reference the project API");
    }

    #[test]
    fn find_chrome_use_returns_option() {
        // Verify the function runs without panic; result depends on the host.
        let _ = find_chrome_use();
    }

    #[test]
    fn ab_eval_double_decode_logic() {
        // Simulate chrome-use output: the page returned JSON.stringify({key:"val"}),
        // so chrome-use printed the JSON-encoded wrapper: "\"{\\\"key\\\":\\\"val\\\"}\"".
        // The decode logic should produce Value::Object({key: "val"}).
        let page_value = serde_json::json!({"key": "val"});
        let page_json = serde_json::to_string(&page_value).unwrap(); // {"key":"val"}
        let chrome_use_line = serde_json::to_string(&page_json).unwrap(); // "\"{...}\""

        // Reproduce the decode loop from ab_eval.
        let inner: serde_json::Value = serde_json::from_str(&chrome_use_line).unwrap();
        assert!(inner.is_string());
        let second: serde_json::Value =
            serde_json::from_str(inner.as_str().unwrap()).unwrap();
        assert_eq!(second["key"], "val");
    }

    #[test]
    fn which_bin_finds_sh_on_unix() {
        // /bin/sh should always exist on Unix.
        let result = which_bin("sh");
        assert!(result.is_some(), "sh should be findable on PATH");
    }
}
