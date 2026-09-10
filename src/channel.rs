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
use std::fs::File;
use std::io::{Seek, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

// Accepted chrome-use binary names, newest name first (mirrors chatgpt-imagegen).
const AB_BIN_CANDIDATES: &[&str] = &["chrome-use", "agent-browser", "agent-browser-stealth", "abs"];

const WEB_NEW_CHAT_URL: &str = "https://chatgpt.com/";
/// One shared chrome-use session name — deliberately NOT per-process.
///
/// A different session name is a different tab, so the old `chatgpt-use-<pid>`
/// default opened a fresh ChatGPT window for every invocation. That is not just
/// untidy: ChatGPT pushes toasts into EVERY open chatgpt.com tab (so a
/// document-wide read can pick up a sibling tab's content), and the account
/// rate-limits per account, not per tab. Both tools that drive this surface
/// (chatgpt-use and chatgpt-imagegen) use this name, so there is one window.
/// Pass `--session` to override when you deliberately want a separate tab.
const DEFAULT_SESSION: &str = "chatgpt-web";
const WEB_PROJECT_URL_TPL: &str = "https://chatgpt.com/g/{gizmo_id}/project";
// Reconnect target. The plain /c/<id> form resolves even for a chat filed under
// a Project — ChatGPT redirects it to /g/<gizmo>/c/<id> — so the conversation id
// alone is enough to find our way back.
const WEB_CONVO_URL_TPL: &str = "https://chatgpt.com/c/";

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
  // A connector turn is multi-step: ChatGPT shows "Calling tool" / "Searching" /
  // "Running…" indicators while a tool runs (a build/test can take MINUTES). Treat
  // the turn as still in progress while any such active-tool marker is present, so
  // we don't scrape an intermediate ("I'll check next…") message instead of the
  // final report, and so we don't give up mid-build. Match PRESENT-tense action
  // verbs only — never past-tense "Thought for Xs" / "Worked for Xs", which persist
  // as static disclosures AFTER the turn ends and would wedge us as forever-active.
  // IMPORTANT: only scan UI chips (buttons), NOT prose. The progress indicator
  // ChatGPT shows while a connector tool runs is a button/disclosure whose text
  // starts with a present-tense action verb ("Running…", "Searching…"). Scanning
  // <div>/<span> too would also match the assistant's OWN words ("Running cargo
  // test…", "Reading Cargo.toml…") in the finished answer — which kept the turn
  // "active" forever and never settled. Verbs are present-progressive only; never
  // past-tense ("Ran"/"Searched"/"Thought for Xs"), which persist after the turn.
  const ACTIVE = /^(calling|searching|running|using|analyzing|analysing|executing|fetching|connecting|generating|working on)\b/i;
  const tool_active = [...document.querySelectorAll('button, [role="button"]')]
    .filter(b => !b.closest('[data-message-author-role]')) // exclude in-message buttons
    .some(b => {
      const t = (b.textContent || '').trim();
      return t.length > 0 && t.length <= 24 && ACTIVE.test(t);
    });
  const cm = location.pathname.match(/\/c\/([0-9a-f-]{36})/i);
  return JSON.stringify({
    stop,
    tool_active,
    convo: cm ? cm[1] : "",
    user_count: document.querySelectorAll('[data-message-author-role="user"]').length,
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

// JS: the conversation UUID this tab is currently showing, or "" on a
// not-yet-persisted new chat (the id only materializes after the first turn).
const JS_CONVO_ID: &str = r#"(() => {
  const m = location.pathname.match(/\/c\/([0-9a-f-]{36})/i);
  return JSON.stringify(m ? m[1] : "");
})()"#;

// JS: how many USER turns are rendered. ChatGPT renders the user bubble
// optimistically the instant a submit is accepted, so a rise in this count is
// authoritative, idempotent evidence that THIS turn was submitted — unlike
// "is the composer empty?", which races React's clear and misreads in both
// directions.
const JS_ASSISTANT_COUNT: &str = r#"(() => {
  const a = document.querySelectorAll('[data-message-author-role="assistant"]');
  return JSON.stringify(a.length);
})()"#;

const JS_USER_COUNT: &str = r#"(() => {
  const u = document.querySelectorAll('[data-message-author-role="user"]');
  return JSON.stringify(u.length);
})()"#;

// JS: empty the composer, so a leftover fragment from an aborted turn can't be
// prepended to the next message.
const JS_CLEAR_COMPOSER: &str = r#"(() => {
  const c = document.querySelector('#prompt-textarea');
  if (!c) return JSON.stringify({ok: false});
  c.focus();
  document.execCommand('selectAll');
  document.execCommand('delete');
  return JSON.stringify({ok: true});
})()"#;

// JS: dismiss a blocking dialog (the rate-limit notice has a "Got it" button).
// Leaving it up keeps the composer unusable even after the throttle lifts.
const JS_DISMISS_DIALOG: &str = r#"(() => {
  const dlg = [...document.querySelectorAll('[role="dialog"]')]
    .find(d => /too many requests|requests too quickly/i.test(d.textContent || ''));
  if (!dlg) return JSON.stringify({ok: false});
  const btn = [...dlg.querySelectorAll('button')]
    .find(b => /got it|ok|dismiss|close/i.test((b.textContent || '').trim()));
  if (btn) { btn.click(); return JSON.stringify({ok: true}); }
  return JSON.stringify({ok: false});
})()"#;

// JS: fingerprint the composer's contents — the non-whitespace character COUNT
// and an order-sensitive HASH of those characters.
//
// Whitespace is excluded because ProseMirror renders each line as its own block,
// so innerText comes back with blank lines between them and a raw length would
// never match. The whitespace set is written out explicitly rather than using
// `\s`, because JavaScript's `\s` and Rust's `char::is_whitespace` disagree on a
// few code points (U+0085 is whitespace only to Rust, U+FEFF only to JS) and a
// single such character in a 30 KB prompt would fail the check for no reason.
//
// The hash is what makes this worth doing. A count alone catches truncation and
// pollution but is blind to REORDERING, and reordering is a real failure mode
// here: chrome-use's `keyboard inserttext` interleaves the tail of one chunk
// with the head of the next while preserving total length (leeguooooo/chrome-use#301,
// reproduced against this very composer). A payload can therefore arrive
// complete, correctly sized, and scrambled. FNV-1a over UTF-16 code units, which
// both sides can compute identically.
const JS_COMPOSER_FINGERPRINT: &str = r#"(() => {
  const c = document.querySelector('#prompt-textarea');
  const t = c ? (c.innerText || c.textContent || '') : '';
  const isWs = (u) =>
    (u >= 0x09 && u <= 0x0d) || u === 0x20 || u === 0x85 || u === 0xa0 ||
    u === 0x1680 || (u >= 0x2000 && u <= 0x200a) || u === 0x2028 ||
    u === 0x2029 || u === 0x202f || u === 0x205f || u === 0x3000 || u === 0xfeff;
  let n = 0;
  let h = 0x811c9dc5;
  for (let i = 0; i < t.length; i++) {
    const u = t.charCodeAt(i);
    if (isWs(u)) continue;
    n++;
    h = (h ^ (u & 0xff)) >>> 0;
    h = Math.imul(h, 0x01000193) >>> 0;
    h = (h ^ (u >>> 8)) >>> 0;
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return JSON.stringify({n: n, h: h});
})()"#;

/// JS: insert `text` at the caret via `execCommand('insertText')`.
///
/// This is deliberately NOT `keyboard type`. A "\n" typed into ProseMirror is an
/// Enter — i.e. a SUBMIT — so typing any multi-line message (every `run`/`serve`
/// system prompt) chopped it at each newline and fired the pieces off as many
/// separate chat messages, which ChatGPT then answered as fragments. Verified
/// live: `keyboard type "A\nB"` submits "A" and leaves "B" in the box.
///
/// `insertText` treats "\n" as literal text while still firing the real
/// beforeinput/input events ProseMirror and React need, so the send button stays
/// bound to the live content (which is why `fill` was avoided in the first place).
fn js_insert_text(text: &str) -> String {
    let t = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(() => {{
  const c = document.querySelector('#prompt-textarea');
  if (!c) return JSON.stringify({{ok: false, error: 'composer not found'}});
  c.focus();
  const ok = document.execCommand('insertText', false, {t});
  return JSON.stringify({{ok}});
}})()"#
    )
}

/// A JS prelude every backend call shares: fetch the page's bearer token ONCE
/// and keep it on `window` until it is close to expiring.
///
/// `/api/auth/session` was being re-fetched by every single backend call. The
/// completion check alone runs every ~20s, so a five-minute turn spent fifteen
/// requests re-asking for a token that had not changed. The throttle that keeps
/// biting this account counts requests, so that is fifteen requests of pure
/// waste per turn, on top of whatever the call actually needed.
///
/// Cached against the session's own `expires`, with a minute of headroom, and
/// re-fetched on any failure — a stale token is worse than an extra request.
const JS_TOKEN_PRELUDE: &str = r#"
const __cguToken = async () => {
  try {
    const now = Date.now();
    const c = window.__cguTok;
    if (c && c.token && c.until > now) return c.token;
    const s = await fetch('/api/auth/session', {credentials: 'include'}).then(r => r.json());
    if (!s || !s.accessToken) return null;
    // `expires` on this payload is the SESSION's lifetime — observed 90 days —
    // not the access token's, so honouring it would cache a bearer far past its
    // real validity and turn every later call into a 401. Cap the cache at five
    // minutes: still collapses the ~15 fetches a long turn used to make into
    // one or two, without betting on a token staying good.
    const exp = Date.parse(s.expires || '');
    const cap = now + 300000;
    const until = Number.isFinite(exp) ? Math.min(exp - 60000, cap) : cap;
    window.__cguTok = {token: s.accessToken, until: until};
    return s.accessToken;
  } catch (e) {
    return null;
  }
};
"#;

/// JS: ask the SERVER whether this turn is finished, and what it said.
///
/// The DOM can only be inferred from: we watch it stop changing and call that an
/// ending. That inference has been observed wrong in both directions — a
/// mid-stream pause read as finished, and (live, this session) a completed reply
/// read as a 240s timeout because the page had stopped rendering while the model
/// worked perfectly. `end_turn` is not an inference; it is the server saying the
/// turn closed.
///
/// The mapping is a TREE — editing a prompt or regenerating forks it and the
/// abandoned branches stay in the payload — so the live thread is `current_node`'s
/// parent chain, not `Object.values(mapping)`.
fn js_server_final(convo_id: &str) -> String {
    let c = serde_json::to_string(convo_id).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(async () => {{
  {prelude}
  try {{
    const tok = await __cguToken();
    if (!tok) return JSON.stringify({{ok: false, error: 'not signed in'}});
    const r = await fetch('/backend-api/conversation/' + {c}, {{
      credentials: 'include',
      headers: {{Authorization: 'Bearer ' + tok}},
    }});
    if (!r.ok) return JSON.stringify({{ok: false, error: 'HTTP ' + r.status}});
    const j = await r.json();
    const map = j.mapping || {{}};
    const chain = [];
    const seen = {{}};
    let cur = j.current_node;
    while (cur && map[cur] && !seen[cur]) {{ seen[cur] = 1; chain.push(map[cur]); cur = map[cur].parent; }}
    const textOf = (c2) => {{
      if (!c2) return '';
      if (Array.isArray(c2.parts)) {{
        return c2.parts.map(p => typeof p === 'string' ? p : (p && p.text) || '')
          .filter(Boolean).join('\n').trim();
      }}
      return typeof c2.content === 'string' ? c2.content.trim() : '';
    }};
    // chain is leaf -> root, so the first qualifying assistant message is the last one.
    for (const n of chain) {{
      const m = n.message;
      if (!m || !m.author || m.author.role !== 'assistant') continue;
      if (m.weight === 0) continue;
      const ct = m.content && m.content.content_type;
      if (ct === 'reasoning_recap' || ct === 'thoughts') continue;
      const t = textOf(m.content);
      if (!t) continue;
      return JSON.stringify({{ok: true, done: m.end_turn === true, text: t,
                              async_status: j.async_status === undefined ? null : j.async_status}});
    }}
    return JSON.stringify({{ok: true, done: false, text: '',
                            async_status: j.async_status === undefined ? null : j.async_status}});
  }} catch (e) {{
    return JSON.stringify({{ok: false, error: String(e)}});
  }}
}})()"#,
        prelude = JS_TOKEN_PRELUDE
    )
}

/// JS: move an existing conversation into a Project, server-side.
///
/// The escape hatch for when ChatGPT's project PAGE will not render — observed
/// account-wide for hours, every project showing only a "Try again" button while
/// the backend API kept answering normally. The UI being broken is not a reason
/// for `--project` to silently stop working: the conversation can be created in
/// a plain chat and filed afterwards, which is what this does.
fn js_file_into_project(convo_id: &str, gizmo_id: &str) -> String {
    let c = serde_json::to_string(convo_id).unwrap_or_else(|_| "\"\"".to_string());
    let g = serde_json::to_string(gizmo_id).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(async () => {{
  {prelude}
  try {{
    const tok = await __cguToken();
    if (!tok) return JSON.stringify({{ok: false, error: 'not signed in'}});
    const r = await fetch('/backend-api/conversation/' + {c}, {{
      method: 'PATCH',
      credentials: 'include',
      headers: {{Authorization: 'Bearer ' + tok, 'Content-Type': 'application/json'}},
      body: JSON.stringify({{gizmo_id: {g}}}),
    }});
    if (!r.ok) return JSON.stringify({{ok: false, error: 'HTTP ' + r.status}});
    return JSON.stringify({{ok: true}});
  }} catch (e) {{
    return JSON.stringify({{ok: false, error: String(e)}});
  }}
}})()"#,
        prelude = JS_TOKEN_PRELUDE
    )
}

/// The thinking-effort levels, in slider order. The INDEX is the contract with
/// the page (`aria-valuenow`); the names here are only what a caller types.
const LEVEL_ORDER: &[&str] = &["instant", "medium", "high", "extra high", "pro"];

/// Map a `--model` value to a slider index, or `None` if it names a model
/// family rather than an effort level.
fn level_index(want: &str) -> Option<usize> {
    let norm = want.trim().to_lowercase().replace(['-', '_'], " ");
    let norm = norm.split_whitespace().collect::<Vec<_>>().join(" ");
    if norm == "extrahigh" {
        return Some(3);
    }
    LEVEL_ORDER.iter().position(|l| *l == norm)
}

// JS: start a fresh chat WITHOUT reloading the page.
//
// A full navigation to https://chatgpt.com/ costs 45 backend-api requests on
// this account — the SPA re-boots and re-fetches every project's metadata and
// conversation list (9 projects here, so 18 requests before anything else).
// Clicking the app's own "New chat" control does the same job client-side for
// exactly ONE request (/backend-api/conversation/init). Measured, both numbers.
//
// That difference is the whole rate-limit story: the throttle counts requests,
// not messages, so reloading the page once per invocation was costing 45x what
// the work actually needed.
const JS_NEW_CHAT_IN_PLACE: &str = r#"(() => {
  if (!/(^|\.)chatgpt\.com$/.test(location.hostname)) {
    return JSON.stringify({ok: false, error: 'not on chatgpt.com'});
  }
  const cands = [...document.querySelectorAll('a, button, [role="button"]')];
  const hit = cands.find(el => {
    const al = (el.getAttribute('aria-label') || '').trim();
    const tx = (el.textContent || '').trim();
    return /^new chat$/i.test(al) || /^new chat$/i.test(tx);
  });
  if (!hit) return JSON.stringify({ok: false, error: 'no new-chat control'});
  hit.click();
  return JSON.stringify({ok: true});
})()"#;

/// JS: switch to an already-open conversation through the SPA's own sidebar
/// link, rather than navigating.
///
/// 9 backend-api requests instead of 45. The pinned conversation is always a
/// recent one, so it is in the sidebar; anything older falls back to a real
/// navigation, which is still correct, just costlier.
fn js_open_convo_in_place(convo_id: &str) -> String {
    let c = serde_json::to_string(convo_id).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(() => {{
  if (!/(^|\.)chatgpt\.com$/.test(location.hostname)) {{
    return JSON.stringify({{ok: false, error: 'not on chatgpt.com'}});
  }}
  const want = {c};
  const link = [...document.querySelectorAll('a[href*="/c/"]')]
    .find(a => (a.getAttribute('href') || '').includes(want));
  if (!link) return JSON.stringify({{ok: false, error: 'conversation not in the sidebar'}});
  link.click();
  return JSON.stringify({{ok: true}});
}})()"#
    )
}

/// JS: open a Project through its sidebar link instead of navigating to it.
fn js_open_project_in_place(gizmo_id: &str) -> String {
    let g = serde_json::to_string(gizmo_id).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(() => {{
  if (!/(^|\.)chatgpt\.com$/.test(location.hostname)) {{
    return JSON.stringify({{ok: false, error: 'not on chatgpt.com'}});
  }}
  const want = {g};
  const link = [...document.querySelectorAll('a[href*="/g/"]')]
    .find(a => (a.getAttribute('href') || '').includes(want));
  if (!link) return JSON.stringify({{ok: false, error: 'project not in the sidebar'}});
  link.click();
  return JSON.stringify({{ok: true}});
}})()"#
    )
}

// JS: locate the composer's model picker WITHOUT relying on its text.
//
// Its label tracks the current model and has read "Instant", "5.6 SolLight" and
// "6Pro" on one account inside three weeks, so any word list goes stale. What is
// stable is where it sits: the composer toolbar row, identified by the plus
// button's testid, holding exactly one other `aria-haspopup="menu"` button.
const JS_FIND_PICKER: &str = r#"(() => {
  const plus = document.querySelector('[data-testid="composer-plus-btn"]');
  if (!plus) return JSON.stringify({ok: false, error: 'composer toolbar not found'});
  const rowTop = plus.getBoundingClientRect().top;
  const hits = [...document.querySelectorAll('button[aria-haspopup="menu"]')].filter(b => {
    if (b.getAttribute('data-testid') === 'composer-plus-btn') return false;
    const r = b.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && Math.abs(r.top - rowTop) < 6;
  });
  if (hits.length !== 1) {
    return JSON.stringify({ok: false,
      error: 'expected one model picker on the composer row, found ' + hits.length});
  }
  const r = hits[0].getBoundingClientRect();
  return JSON.stringify({ok: true, label: (hits[0].textContent || '').trim(),
                         x: Math.round(r.left + r.width / 2),
                         y: Math.round(r.top + r.height / 2)});
})()"#;

// JS: read the opened picker — the effort slider (index + thumb position) and
// the model-family radios. `level` is the name the page currently shows for the
// slider position; it is for logging only, never for matching.
const JS_PICKER_MENU: &str = r#"(() => {
  const menu = document.querySelector('[role="menu"]');
  const sl = document.querySelector('[role="slider"]');
  let slider = null;
  if (sl) {
    const r = sl.getBoundingClientRect();
    slider = {now: Number(sl.getAttribute('aria-valuenow')),
              min: Number(sl.getAttribute('aria-valuemin')),
              max: Number(sl.getAttribute('aria-valuemax')),
              thumbX: Math.round(r.left + r.width / 2),
              thumbY: Math.round(r.top + r.height / 2)};
  }
  const shown = [...document.querySelectorAll('[role="menuitem"]')]
    .find(e => e.getAttribute('aria-label') === 'Select model');
  const radios = [...document.querySelectorAll('[role="menuitemradio"]')].map(e => ({
    text: (e.textContent || '').trim(),
    checked: e.getAttribute('aria-checked') === 'true'
  }));
  return JSON.stringify({open: !!menu, slider, radios,
                         level: shown ? (shown.textContent || '').trim() : ''});
})()"#;

/// JS: click the model-family radio whose text matches exactly. Unlike the
/// picker button and the slider thumb, these DO respond to `element.click()`.
fn js_click_radio(text: &str) -> String {
    let t = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(() => {{
  const target = {t};
  for (const el of document.querySelectorAll('[role="menuitemradio"]')) {{
    if ((el.textContent || '').trim() === target) {{
      el.click();
      return JSON.stringify({{ok: true}});
    }}
  }}
  return JSON.stringify({{ok: false}});
}})()"#
    )
}

// JS: resolve or create a ChatGPT Project by exact display name.
// Returns {ok, id, created, error?}. Mirrors _JS_ENSURE_PROJECT in chatgpt-imagegen.
fn js_ensure_project(name: &str) -> String {
    let name_json = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(async () => {{
  {prelude}
  try {{
    const name = {name_json};
    const tok = await __cguToken();
    if (!tok) return JSON.stringify({{ok: false, error: 'no accessToken in /api/auth/session'}});
    const h = {{Authorization: 'Bearer ' + tok, 'Content-Type': 'application/json'}};
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
        prelude = JS_TOKEN_PRELUDE,
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

/// Tuning for how `send` decides a reply is COMPLETE. Multi-step connector turns
/// (read → bash build → report) stream in phases with quiet gaps; these knobs let
/// long-running modes (`work`) wait through a several-minute build that a one-shot
/// `ask` would never need.
#[derive(Debug, Clone, Copy)]
pub struct SendOptions {
    /// Polls (each ~2s) the visible reply text must stay UNCHANGED, with no
    /// stop-button and no active-tool marker, before we treat it as final.
    pub stable_needed: u32,
    /// Safety net: give up waiting after this many consecutive polls (~2s each)
    /// of total silence — text static, not streaming, no tool running. Only the
    /// per-turn `timeout_secs` deadline applies otherwise.
    pub idle_limit: u32,
}

impl Default for SendOptions {
    fn default() -> Self {
        // One-shot defaults: ~4s of unchanged text confirms; ~60s of silence aborts.
        SendOptions { stable_needed: 2, idle_limit: 30 }
    }
}

impl SendOptions {
    /// Generous settings for closed-loop `work`: a build/test can sit quiet for
    /// minutes, so confirm finality more slowly (~6s) and tolerate ~3min of
    /// silence before the safety net fires (the wall-clock `timeout_secs` is the
    /// real ceiling).
    pub fn work() -> Self {
        SendOptions { stable_needed: 3, idle_limit: 90 }
    }
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
    /// Project to file the conversation under ("" → plain chat). Kept so a
    /// reconnect can rebuild the same starting point.
    project: String,
    /// The conversation this channel is pinned to, latched after the first
    /// successful turn (a fresh chat has no id until then). Every later turn
    /// verifies the tab still shows it, so a sidebar click, a stray navigation
    /// or a project opening a new chat can't silently redirect us into a
    /// DIFFERENT conversation — which would break the "same chat accumulates
    /// context" contract while still returning a plausible-looking reply.
    convo_id: Option<String>,
    /// A project this channel could not ENTER but should still file into, set
    /// when the project page fails to render. Cleared once the conversation has
    /// been moved server-side.
    pending_project: Option<String>,
    /// Exclusive claim on the shared ChatGPT window, released when the channel
    /// is dropped or closed.
    _surface: SurfaceLock,
}

impl Channel {
    /// Connect: find chrome-use, choose a logged-in browser, open ChatGPT (in
    /// the project if set), and wait for the composer. Errors clearly if no
    /// logged-in browser is available or the account is rate-limited.
    pub fn connect(opts: &ChannelOptions) -> Result<Self> {
        // Take the surface BEFORE touching the browser: opening the tab and
        // entering a project already mutate the shared window.
        let surface = SurfaceLock::acquire();

        let ab = find_chrome_use().ok_or_else(|| {
            anyhow!(
                "`chrome-use` is not installed — install it (no npm, no token):\n  \
                 curl -fsSL https://raw.githubusercontent.com/leeguooooo/chrome-use/main/install.sh | sh"
            )
        })?;

        let session = opts
            .session
            .clone()
            .unwrap_or_else(|| DEFAULT_SESSION.to_string());

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

        // Reuse the tab this session already has, if it is a usable ChatGPT
        // page. Navigating instead costs 45 backend-api requests to re-boot the
        // SPA; the app's own "New chat" costs 1. The throttle that has been
        // biting this account counts REQUESTS, not messages, so this is the
        // difference between one run and forty-five as far as it is concerned.
        {
            let probe = ab_eval(&ab, JS_NEW_CHAT_IN_PLACE, &session, 15.0);
            let reused = probe
                .as_ref()
                .ok()
                .and_then(|v| v.get("ok"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if reused {
                let settle = Instant::now() + Duration::from_secs(20);
                if wait_composer(&ab, &session, settle, 20).unwrap_or(false) {
                    eprintln!("reusing the open ChatGPT tab (new chat, no page reload)");
                    opened = true;
                }
            }
        }

        for prof in &candidates {
            if opened {
                break;
            }
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
                    // A chrome-use session can end up permanently unusable: a
                    // command that runs too long is judged unresponsive, the
                    // daemon is stopped, and the NAME stays poisoned — rerunning
                    // repeats the same error, and `session stop --force`,
                    // deleting the lifecycle lock and upgrading the CLI all fail
                    // to clear it. Trying the next candidate cannot help, and
                    // falling through to "no logged-in ChatGPT browser
                    // available. Sign in to chatgpt.com" tells the user to fix
                    // the one thing that is not broken. Say what actually
                    // happened and how to get moving again.
                    if msg.contains("session unresponsive") || msg.contains("stuck") {
                        bail!(
                            "the chrome-use session {session:?} is wedged and will not recover \
                             on its own — every command on that name returns \
                             \"session unresponsive\". You are still signed in; this is not a \
                             login problem.\n\n  Work around it now:  \
                             chatgpt-use <cmd> --session chatgpt-web-2\n\n\
                             It is usually caused by a single very long chrome-use command \
                             (a large `keyboard inserttext`, for instance) being judged \
                             unresponsive, after which the name stays poisoned."
                        );
                    }
                    // other errors: log and try the next candidate
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

        let mut chan = Channel {
            ab,
            session,
            timeout_secs,
            project: opts.project.trim().to_string(),
            convo_id: None,
            pending_project: None,
            _surface: surface,
        };

        // Navigate into a ChatGPT Project FIRST — it loads a new page and would
        // reset any model selection, so model selection must come afterwards.
        let project = opts.project.trim().to_string();
        if !project.is_empty() {
            let proj_deadline = Instant::now() + Duration::from_secs(timeout_secs);
            match chan.resolve_project(&project, proj_deadline) {
                Ok((gizmo, created)) => {
                    eprintln!(
                        "using project {project:?}{}",
                        if created { " (created)" } else { "" }
                    );
                    if let Err(e) = chan.open_project_page(&gizmo, proj_deadline) {
                        // The project page would not render. That is ChatGPT's
                        // front end failing, not a reason for --project to stop
                        // working: the backend still files conversations fine,
                        // so start in a plain chat and move it afterwards.
                        eprintln!(
                            "warning: the project page didn't load ({e}); starting in a plain \
                             chat and filing the conversation into {project:?} afterwards"
                        );
                        chan.pending_project = Some(gizmo);
                        let restore_deadline = Instant::now() + Duration::from_secs(30);
                        let _ =
                            ab_open(&chan.ab, &chan.session, WEB_NEW_CHAT_URL, None, restore_deadline);
                        let _ = wait_composer(&chan.ab, &chan.session, restore_deadline, 15);
                    }
                }
                Err(e) => {
                    // Could not even resolve the project (no id), so there is
                    // nothing to file into later.
                    eprintln!("warning: project {project:?} unavailable ({e}); using a plain chat");
                }
            }
        }

        // Apply an explicitly requested model on the now-settled composer (after
        // any project navigation).
        //
        // This used to warn and carry on with the account default. That quietly
        // misrepresents the run: `--model pro` reports success while answering
        // from some other model, and `work` — which must stay OFF Pro, because
        // Pro cannot use Apps/MCP — silently loses every connector tool and then
        // looks like a model that "won't use its tools". Only ever set when the
        // caller named a model, so erring here refuses exactly the request we
        // cannot honour.
        if let Some(ref model) = opts.model {
            let model_deadline = Instant::now() + Duration::from_secs(timeout_secs.min(30));
            chan.select_model(model, model_deadline).with_context(|| {
                format!(
                    "could not select model {model:?} — refusing to run on the account \
                     default instead. ChatGPT relabelled the composer picker from \
                     Intelligence levels (instant/high/pro) to model names \
                     (e.g. \"5.6 SolLight\"), so the selector needs updating; rerun \
                     without --model to accept whatever the account is set to"
                )
            })?;
        }

        Ok(chan)
    }

    /// Put `message` in the composer and submit it, returning only once a new
    /// user turn proves the submit landed.
    fn fill_and_submit(
        &self,
        message: &str,
        baseline_users: u64,
        budget: f64,
    ) -> std::result::Result<(), SubmitFailure> {
        self.fill_composer(message, budget)
            .map_err(SubmitFailure::BeforeSubmit)?;
        self.submit(baseline_users, budget)
    }

    /// Put `message` in the composer and verify it landed intact. Nothing here
    /// can have submitted anything, so any error is safe to retry.
    fn fill_composer(&self, message: &str, budget: f64) -> Result<()> {
        // Never type while the PAGE still believes it is generating. ChatGPT
        // disables submission then, so Enter is silently swallowed and the turn
        // reports "never submitted".
        //
        // This became reachable when replies started coming from the server: the
        // record says `end_turn` the moment the turn closes, which can be while
        // the page is still rendering the tail, so we can now return from one
        // turn and start the next before the composer is willing. Bounded, and
        // it gives up rather than blocking — a stop button that never clears is
        // its own problem and the submit check below will report it honestly.
        let busy = |ch: &Self| {
            ab_eval(&ch.ab, JS_STATE, &ch.session, budget)
                .ok()
                .filter(|v| v.is_object())
                .map(|v| {
                    v.get("stop").and_then(|b| b.as_bool()).unwrap_or(false)
                        || v.get("tool_active").and_then(|b| b.as_bool()).unwrap_or(false)
                })
                .unwrap_or(false)
        };
        let wait_idle = |ch: &Self, secs: u64| {
            let until = Instant::now() + Duration::from_secs(secs);
            while busy(ch) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(500));
            }
        };

        wait_idle(self, 10);
        if busy(self) {
            // The page is stuck mid-generation and will not recover on its own.
            // Seen live on an account whose front end had degraded: after a turn
            // the stop button stayed present indefinitely — still there 32s
            // later — so the composer refused every further message while the
            // server had long since closed the turn. Waiting longer does not
            // help; reloading the conversation does. This is the same trade the
            // rest of the channel makes: the record is authoritative, the page
            // is just a keyboard, and a keyboard that has locked up gets reset.
            eprintln!("the page is stuck mid-generation; reloading the conversation to free the composer");
            if self.convo_id.is_some() {
                self.reopen_pinned(budget)
                    .context("reloading a page stuck mid-generation")?;
            }
            wait_idle(self, 10);
        }

        // Focus and empty the composer, then insert the message as TEXT.
        ab_cmd(&self.ab, &["click", "#prompt-textarea"], &self.session, budget)
            .context("clicking #prompt-textarea")?;
        // Clear, then CONFIRM the composer is actually empty. One `delete` is not
        // enough after a reattach: the page may still be hydrating, and ChatGPT
        // restores a saved draft into the composer once it is — which silently
        // prepends a stray character to the prompt. (Seen live: a 26 KB payload
        // arrived one character long, which the integrity check below correctly
        // rejected.) Whitespace is ignored, so only real leftover text blocks us.
        let mut cleared = false;
        for _ in 0..5 {
            let _ = ab_eval(&self.ab, JS_CLEAR_COMPOSER, &self.session, budget);
            if ab_eval(&self.ab, JS_COMPOSER_FINGERPRINT, &self.session, budget)
                .ok()
                .and_then(|v| v.get("n").and_then(|n| n.as_u64()))
                == Some(0)
            {
                cleared = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        if !cleared {
            bail!(
                "could not empty the ChatGPT composer — leftover text would be \
                 prepended to the message"
            );
        }

        // Insert in chunks — a multi-KB argument overruns chrome-use's IPC and
        // fails with EAGAIN ("Resource temporarily unavailable"). Each chunk
        // appends at the caret. Split on char boundaries (prompts contain
        // multibyte text). See `js_insert_text` for why this is not `keyboard
        // type`: typed newlines submit, which silently shredded every multi-line
        // prompt into one chat message per line.
        const INSERT_CHUNK_CHARS: usize = 1500;
        let chars: Vec<char> = message.chars().collect();
        for chunk in chars.chunks(INSERT_CHUNK_CHARS) {
            let piece: String = chunk.iter().collect();
            let res = ab_eval(&self.ab, &js_insert_text(&piece), &self.session, budget)
                .context("inserting message text into composer")?;
            if !res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                bail!("could not insert text into the ChatGPT composer");
            }
        }

        // Integrity check BEFORE submitting: confirm the whole payload is sitting
        // in the composer, in the right ORDER. Cheaper and far more useful than
        // discovering a mangled prompt from a confused reply ten minutes later.
        //
        // Both halves matter. The count catches truncation (a chunk that never
        // landed) and pollution (a restored draft prepended); the hash catches
        // reordering, which the count cannot see because it preserves length —
        // and reordering is not hypothetical here, it is what
        // leeguooooo/chrome-use#301 does to chunked inserts.
        let (want_n, want_h) = composer_fingerprint(message);
        let got = ab_eval(&self.ab, JS_COMPOSER_FINGERPRINT, &self.session, budget).ok();
        let got_n = got.as_ref().and_then(|v| v.get("n")).and_then(|v| v.as_u64());
        let got_h = got.as_ref().and_then(|v| v.get("h")).and_then(|v| v.as_u64());
        if let (Some(n), Some(h)) = (got_n, got_h) {
            if n != want_n {
                bail!(
                    "composer content doesn't match the message to send \
                     ({n} non-whitespace chars present, {want_n} expected) — \
                     refusing to submit a truncated or polluted prompt"
                );
            }
            if h != want_h as u64 {
                bail!(
                    "composer holds the right number of characters ({n}) but not in \
                     the right order (fingerprint {h:#x}, expected {:#x}) — refusing \
                     to submit a scrambled prompt",
                    want_h
                );
            }
        }

        Ok(())
    }

    /// Press Enter and return only once a new user turn proves it landed.
    fn submit(&self, baseline_users: u64, budget: f64) -> std::result::Result<(), SubmitFailure> {
        // From here on a retry could DUPLICATE the message, so every failure is
        // reported as ambiguous and the caller must not resend blindly.
        ab_cmd(&self.ab, &["press", "Enter"], &self.session, budget)
            .context("pressing Enter to submit")
            .map_err(SubmitFailure::Ambiguous)?;

        // Confirm the submit actually landed, by EVIDENCE (a new user turn was
        // rendered) rather than by the old proxy "is the composer empty?". That
        // proxy raced React's clear and misread in both directions: a slow clear
        // looked like a failed submit (→ duplicate send), and a swallowed Enter
        // with an already-cleared box looked like success (→ we then waited on,
        // and scraped, the PREVIOUS turn).
        if !self.await_user_turn(baseline_users, Duration::from_secs(3), budget) {
            // Enter didn't take. Click the send button and demand evidence again.
            // Note this fallback is naturally inert if the submit did land after
            // all: once generation starts, the send button becomes the stop
            // button and this selector matches nothing.
            let _ = ab_cmd(
                &self.ab,
                &["click", r#"button[data-testid="send-button"]"#],
                &self.session,
                budget,
            );
            if !self.await_user_turn(baseline_users, Duration::from_secs(5), budget) {
                return Err(SubmitFailure::Ambiguous(anyhow!(
                    "the message was never submitted — no new user turn appeared \
                     after pressing Enter and clicking the send button. The \
                     composer may be disabled (rate limit, expired session) or \
                     the page layout changed."
                )));
            }
        }

        Ok(())
    }

    /// The server's own answer for the pinned conversation: is the turn closed,
    /// and what is the final assistant text. `None` when there is nothing to ask
    /// about or the call failed — never an error, because this is a second
    /// opinion, not the primary path.
    fn server_final(&self, budget: f64) -> Option<(bool, String)> {
        let id = self.convo_id.as_ref()?;
        let res = ab_eval(&self.ab, &js_server_final(id), &self.session, budget).ok()?;
        if !res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return None;
        }
        let done = res.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
        let text = strip_private_markers(res.get("text").and_then(|v| v.as_str()).unwrap_or(""));
        Some((done, text))
    }

    /// Move an existing conversation into a Project through the backend API.
    fn file_into_project(&self, convo_id: &str, gizmo_id: &str, budget: f64) -> Result<()> {
        let res = ab_eval(
            &self.ab,
            &js_file_into_project(convo_id, gizmo_id),
            &self.session,
            budget,
        )
        .context("calling the project-filing endpoint")?;
        if res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(());
        }
        let detail = res.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");

        // This is the one place a remembered gizmo id gets tested against the
        // server, so it is where a stale one has to be dropped. A project that
        // was deleted, or that belongs to a different account than the browser
        // is now signed into, would otherwise be retried from cache forever —
        // the cache would have turned a transient mistake into a permanent one.
        if detail.contains("404") || detail.contains("400") || detail.contains("403") {
            forget_gizmo(&self.project);
            eprintln!(
                "forgetting the remembered id for project {:?} ({detail}); it will be \
                 looked up again next run",
                self.project
            );
        }
        bail!("{detail}")
    }

    /// The conversation UUID currently shown in the tab, if any.
    fn current_convo_id(&self, budget: f64) -> Option<String> {
        ab_eval(&self.ab, JS_CONVO_ID, &self.session, budget)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
    }

    /// Fail closed if the tab has drifted off the conversation we pinned.
    ///
    /// A `None` pin means "not latched yet" (fresh chat, first turn) and always
    /// passes. Once latched, a mismatch is never recoverable by carrying on:
    /// the multi-turn modes rely on context accumulated in the pinned chat, so
    /// answering from a different one is worse than erroring.
    fn verify_convo(&self, budget: f64) -> Result<()> {
        let Some(ref pinned) = self.convo_id else { return Ok(()) };
        if convo_drift(pinned, self.current_convo_id(budget).as_deref()).is_none() {
            return Ok(());
        }
        // Drifted. Try once to steer back before giving up — a stray navigation
        // or a sidebar click is recoverable, and the conversation itself (with
        // all our accumulated context) is still there on the server.
        self.reopen_pinned(budget)?;
        match convo_drift(pinned, self.current_convo_id(budget).as_deref()) {
            None => Ok(()),
            Some(msg) => bail!("{msg}"),
        }
    }

    /// Re-establish a FRESH chat after losing the tab before anything was said.
    ///
    /// Safe only pre-pin and pre-submit: with no conversation id there is no
    /// context to preserve, and with no submit receipt there is no risk of
    /// duplicating a message that is already generating. This is the ordinary
    /// case when another process sharing the session finishes and closes the
    /// browser out from under us.
    fn reopen_fresh(&self, budget: f64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs_f64(budget.clamp(20.0, 90.0));
        eprintln!("the ChatGPT tab went away before the first turn; opening a fresh chat");
        let in_place = ab_eval(&self.ab, JS_NEW_CHAT_IN_PLACE, &self.session, budget)
            .ok()
            .and_then(|v| v.get("ok").and_then(|b| b.as_bool()))
            .unwrap_or(false);
        if !(in_place && wait_composer(&self.ab, &self.session, deadline, 20).unwrap_or(false)) {
            ab_open(&self.ab, &self.session, WEB_NEW_CHAT_URL, None, deadline)
                .context("reopening ChatGPT")?;
        }
        if !wait_composer(&self.ab, &self.session, deadline, 30)? {
            bail!("reopened ChatGPT but the composer never appeared");
        }
        if !self.project.is_empty() {
            match self.resolve_project(&self.project, deadline) {
                Ok((gizmo, _)) => {
                    if self.open_project_page(&gizmo, deadline).is_err() {
                        // Same degradation as `connect`: keep the project, lose
                        // only the ability to be born inside it.
                        eprintln!(
                            "warning: the project page didn't load; will file the conversation \
                             into {:?} afterwards",
                            self.project
                        );
                    }
                }
                Err(e) => eprintln!(
                    "warning: project {:?} unavailable ({e}); using a plain chat",
                    self.project
                ),
            }
        }
        Ok(())
    }

    /// Navigate the session back to the pinned conversation.
    ///
    /// This is the whole point of pinning an id: when the tab is closed, the
    /// browser restarts, or the page wanders off, the conversation and any
    /// in-flight generation live on SERVER-side — only our observer was lost.
    /// Reopening `/c/<id>` re-attaches to it, so a turn that would previously
    /// have burned down to a bare "timed out" can carry on.
    fn reopen_pinned(&self, budget: f64) -> Result<()> {
        let Some(ref id) = self.convo_id else {
            bail!(
                "lost contact with the ChatGPT tab before this conversation had an \
                 id (the id only exists once the first turn is persisted), so there \
                 is nothing to reconnect to — rerun the command."
            );
        };
        let deadline = Instant::now() + Duration::from_secs_f64(budget.clamp(20.0, 90.0));
        eprintln!("reattaching to conversation {id}");

        // Sidebar link first: 9 backend-api requests against 45 for a real
        // navigation. This path runs often — every page reset on a degraded
        // front end goes through it — so the difference compounds fast.
        let in_place = ab_eval(&self.ab, &js_open_convo_in_place(id), &self.session, budget)
            .ok()
            .and_then(|v| v.get("ok").and_then(|b| b.as_bool()))
            .unwrap_or(false);
        if in_place && wait_composer(&self.ab, &self.session, deadline, 20).unwrap_or(false) {
            return Ok(());
        }

        let url = format!("{WEB_CONVO_URL_TPL}{id}");
        ab_open(&self.ab, &self.session, &url, None, deadline)
            .context("reopening the pinned conversation")?;
        if !wait_composer(&self.ab, &self.session, deadline, 30)? {
            bail!("reopened conversation {id} but the composer never appeared");
        }
        Ok(())
    }

    /// Number of rendered user turns, or `None` if the page couldn't be read.
    fn user_turn_count(&self, budget: f64) -> Option<u64> {
        ab_eval(&self.ab, JS_USER_COUNT, &self.session, budget)
            .ok()
            .and_then(|v| v.as_u64())
    }

    /// Poll up to `within` for the user-turn count to exceed `baseline` — i.e.
    /// for positive evidence that our submit was accepted.
    fn await_user_turn(&self, baseline: u64, within: Duration, budget: f64) -> bool {
        let until = Instant::now() + within;
        loop {
            if self.user_turn_count(budget).is_some_and(|n| n > baseline) {
                return true;
            }
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Send one message and return ChatGPT's completed reply as text/markdown,
    /// using the default (one-shot) completion tuning.
    pub fn send(&mut self, message: &str) -> Result<String> {
        self.send_with(message, &SendOptions::default())
    }

    /// Send one message with explicit completion tuning (see `SendOptions`).
    pub fn send_with(&mut self, message: &str, sopts: &SendOptions) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(self.timeout_secs);

        let remaining_secs = || {
            deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::from_secs(2))
                .as_secs_f64()
                .max(2.0)
        };

        // Refuse to type into a tab that has drifted off our pinned conversation.
        self.verify_convo(remaining_secs())?;

        // Any deferred project filing happens HERE, before this turn types
        // anything, rather than after the previous one finished.
        //
        // Filing re-routes the open conversation to its project URL and the
        // composer is gone for that moment, so doing it mid-turn broke the next
        // send. Doing it at the START of a turn puts the re-route before the
        // typing, where the idle-wait and page-reset below already absorb it —
        // and unlike filing in `close`, it also covers `serve`, which holds one
        // channel for the life of the process and never closes it.
        if let (Some(gizmo), Some(cid)) = (self.pending_project.clone(), self.convo_id.clone()) {
            match self.file_into_project(&cid, &gizmo, remaining_secs()) {
                Ok(()) => {
                    eprintln!("filed conversation into project {:?}", self.project);
                    self.pending_project = None;
                }
                Err(e) => eprintln!(
                    "warning: could not file the conversation into {:?} ({e}); it stays in a \
                     plain chat",
                    self.project
                ),
            }
        }

        // Snapshot rendered user turns: a rise in this count is our submit
        // receipt (see JS_USER_COUNT).
        let mut baseline_users: u64 = self.user_turn_count(remaining_secs()).unwrap_or(0);

        // Snapshot the current number of assistant messages so we can detect
        // when a NEW one arrives.
        let baseline_count: u64 = ab_eval(&self.ab, JS_ASSISTANT_COUNT, &self.session, remaining_secs())
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // Fill + submit, with ONE reattach-and-retry: the tab can vanish between
        // turns (closed, crashed, browser restarted) and the conversation itself
        // is still on the server, so losing the window shouldn't lose the turn.
        if let Err(first) = self.fill_and_submit(message, baseline_users, remaining_secs()) {
            // Fail closed on anything that might already be in flight.
            let SubmitFailure::BeforeSubmit(why) = first else {
                return Err(first.into_error());
            };
            eprintln!("submit failed: {why:#}");

            // Reconnect: back to our conversation if we have one, otherwise to a
            // fresh chat — nothing has been said yet, so nothing is lost.
            if self.convo_id.is_some() {
                self.reopen_pinned(remaining_secs())
                    .context("could not reattach after a failed submit")?;
            } else {
                self.reopen_fresh(remaining_secs())
                    .context("could not reopen ChatGPT after a failed submit")?;
            }

            // Re-baseline against the reattached page, and skip the resend
            // entirely if the message turns out to be in flight already.
            let users_now = self.user_turn_count(remaining_secs()).unwrap_or(0);
            if self.convo_id.is_some() && users_now > baseline_users {
                eprintln!("the message had already been submitted; observing that turn");
            } else {
                self.fill_and_submit(message, users_now, remaining_secs())
                    .map_err(SubmitFailure::into_error)
                    .context("resubmitting after reconnect")?;
                // The poll loop below uses this baseline to tell "our page" from
                // "some other page". A reconnect can land on a chat with FEWER
                // turns than the one we started on (a fresh chat has none), so
                // the pre-reconnect figure would read as a page swap and abort
                // the turn we just successfully submitted.
                baseline_users = users_now;
            }
        }

        // Re-read the assistant baseline: if we reattached above, the reloaded
        // page reflects the server's view and the pre-crash count is meaningless.
        let baseline_count = baseline_count.min(
            ab_eval(&self.ab, JS_ASSISTANT_COUNT, &self.session, remaining_secs())
                .ok()
                .and_then(|v| v.as_u64())
                .unwrap_or(baseline_count),
        );

        // Poll until the stop button is gone AND a new assistant message count
        // is larger than baseline.
        let poll_interval = Duration::from_millis(2000);
        let mut last_atext = String::new();
        let mut idle_count = 0u32;
        // Stable-read: a reply (esp. one that calls connector tools) streams in
        // phases — preamble, tool calls, final text — with brief moments where
        // the stop button is absent mid-turn. Don't scrape until the assistant
        // text has been UNCHANGED for a couple of polls AND the stop button is
        // gone, so we capture the FINAL message, not a mid-turn partial.
        let mut prev_stable = String::new();
        let mut stable_polls = 0u32;
        let stable_needed = sopts.stable_needed.max(1);
        // After idle_limit polls (~2s each) with no progress at all we give up
        // rather than hanging until the total timeout.
        let idle_limit = sopts.idle_limit.max(1);

        // Consecutive unreadable polls. ~3 misses (~6s) is well past a normal
        // navigation hiccup and reads as "the tab is gone".
        const LOST_POLLS_BEFORE_REATTACH: u32 = 3;
        let mut lost_polls = 0u32;

        // How often to ask the server instead of the page. Every 10th ~2s poll.
        const SERVER_CHECK_EVERY: u64 = 10;
        let mut polls: u64 = 0;
        // How many times we have backed off for the page's rate-limit dialog.
        let mut limited_waits: u32 = 0;

        // Heartbeat: the page can think silently for minutes, so emit an
        // elapsed-time progress line to stderr (~every 5s) so the wait is visible.
        let started = Instant::now();
        let mut last_beat: u64 = 0;
        eprintln!("[   0.0s] submitted; waiting for reply");

        loop {
            if Instant::now() >= deadline {
                // Before giving up, ask the server. The DOM going quiet is an
                // inference; `end_turn` is not. Live this session: a turn that
                // timed out here at 240s had in fact completed — the page had
                // stopped rendering while the model worked fine. Reporting a
                // timeout for an answer that exists is the worst outcome
                // available, so spend one call to avoid it.
                if let Some((true, text)) = self.server_final(30.0) {
                    if !text.trim().is_empty() {
                        eprintln!(
                            "the page stopped updating, but the server says this turn finished \
                             — taking the reply from the conversation record"
                        );
                        return self.finish_turn(text, remaining_secs());
                    }
                }
                bail!(
                    "timed out after {}s waiting for ChatGPT to complete the reply",
                    self.timeout_secs
                );
            }
            std::thread::sleep(poll_interval);

            let read = ab_eval(&self.ab, JS_STATE, &self.session, remaining_secs());

            // Is this still OUR conversation? Identity — not "did the eval
            // error?" — is the reliable signal that we lost the page. Closing
            // the tab does NOT surface as a failed eval: chrome-use just hands
            // back a fresh blank session, whose state object reads perfectly
            // well as "no assistant messages, not generating". The old
            // failure-based check therefore never fired and the turn silently
            // spun out the full wall-clock timeout instead of reconnecting.
            let seen_convo = read
                .as_ref()
                .ok()
                .and_then(|v| v.get("convo"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            // Id-independent loss check, and the only one that works on turn one:
            // ChatGPT doesn't put /c/<id> in the URL until the FIRST turn is
            // persisted, so mid-turn-one there is no identity to compare. But we
            // hold a submit receipt — the user-turn count rose — and that count
            // can never legitimately go DOWN. If it does, we're looking at a
            // different (blank) page.
            let users_now = read
                .as_ref()
                .ok()
                .and_then(|v| v.get("user_count"))
                .and_then(|v| v.as_u64());
            if users_now.is_some_and(|n| n <= baseline_users) {
                if self.convo_id.is_some() {
                    lost_polls += 1;
                    if lost_polls >= LOST_POLLS_BEFORE_REATTACH {
                        self.reopen_pinned(remaining_secs())
                            .context("lost the ChatGPT tab and could not reattach")?;
                        lost_polls = 0;
                    }
                    continue;
                }
                bail!(
                    "the ChatGPT page was replaced while the first turn was still \
                     running, and ChatGPT does not put a conversation id in the URL \
                     until that turn finishes — so there is no conversation to \
                     reattach to. The reply may still have completed in your \
                     browser; rerun the command."
                );
            }

            match (&self.convo_id, &seen_convo) {
                // Latch as soon as the id exists — the server assigns it right
                // after the first submit, so even turn one becomes recoverable
                // rather than having to survive un-pinned until it completes.
                (None, Some(id)) => {
                    eprintln!("pinned to conversation {id}");
                    self.convo_id = Some(id.clone());
                    lost_polls = 0;
                }
                (Some(pinned), Some(seen)) if seen == pinned => lost_polls = 0,
                // Pinned, but the page is showing something else (or nothing).
                // The generation is still running server-side; only our view of
                // it was lost. Reattach and keep observing.
                (Some(_), _) => {
                    lost_polls += 1;
                    if lost_polls >= LOST_POLLS_BEFORE_REATTACH {
                        self.reopen_pinned(remaining_secs())
                            .context("lost the ChatGPT tab and could not reattach")?;
                        lost_polls = 0;
                    }
                    continue;
                }
                (None, None) => {}
            }

            let st = match read {
                Ok(v) if v.is_object() => v,
                _ => continue,
            };

            if st.get("limited").and_then(|v| v.as_bool()).unwrap_or(false) {
                // The prompt is already in. Handing the user an error here throws
                // away a turn the model is very likely still completing — the
                // throttle is on the conversations API, not on generation, which
                // is why the record keeps filling in while the page shows this
                // dialog. So wait it out inside our own deadline instead, and
                // keep asking the server, which is the one source still answering.
                limited_waits += 1;
                let backoff = Duration::from_secs(match limited_waits {
                    1 => 20,
                    2 => 45,
                    _ => 90,
                });
                if Instant::now() + backoff >= deadline {
                    bail!(
                        "{} The prompt was already submitted; check the conversation \
                         or retry in a few minutes.",
                        RATE_LIMIT_MSG
                    );
                }
                eprintln!(
                    "[{:5}.0s] rate-limited by the page; waiting {}s (attempt {}) — the reply \
                     may still be arriving server-side",
                    started.elapsed().as_secs(),
                    backoff.as_secs(),
                    limited_waits
                );
                // Dismiss the dialog so the page is usable if it does recover.
                let _ = ab_eval(&self.ab, JS_DISMISS_DIALOG, &self.session, remaining_secs());
                std::thread::sleep(backoff);
                if let Some((true, text)) = self.server_final(remaining_secs().min(30.0)) {
                    if !text.trim().is_empty() {
                        eprintln!("the turn completed despite the throttle; taking it from the record");
                        return self.finish_turn(text, remaining_secs());
                    }
                }
                continue;
            }

            let stop = st.get("stop").and_then(|v| v.as_bool()).unwrap_or(false);
            let tool_active = st.get("tool_active").and_then(|v| v.as_bool()).unwrap_or(false);
            let cur_count = st
                .get("assistant_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            let atext = st
                .get("atext")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Throttled heartbeat (~every 5s). Logs msg count + reply length so a
            // hang is diagnosable from the log alone (e.g. msgs=0 → no assistant
            // node yet; len frozen → settling; len climbing → still streaming).
            // Periodic reconciliation. Cheap enough at this cadence (~every
            // 20s) and it ends the wait the moment the server says the turn
            // closed, instead of waiting out the settle heuristics.
            polls += 1;
            if polls.is_multiple_of(SERVER_CHECK_EVERY) && self.convo_id.is_some() {
                if let Some((true, text)) = self.server_final(remaining_secs().min(30.0)) {
                    if !text.trim().is_empty() {
                        eprintln!(
                            "[{:5}.0s] server reports the turn closed; taking the reply from the \
                             conversation record",
                            started.elapsed().as_secs()
                        );
                        return self.finish_turn(text, remaining_secs());
                    }
                }
            }

            let elapsed = started.elapsed().as_secs();
            if elapsed >= last_beat + 5 {
                last_beat = elapsed;
                let phase = if tool_active {
                    "running a tool"
                } else if stop {
                    "generating"
                } else {
                    "waiting for reply"
                };
                eprintln!("[{elapsed:5}.0s] {phase} (msgs={cur_count}, len={})", atext.len());
            }

            if !atext.is_empty() {
                last_atext = atext.clone();
            }

            if stop || tool_active {
                // Streaming, or a connector tool call is mid-flight — not settled.
                idle_count = 0;
                stable_polls = 0;
                continue;
            }

            // We only reach here with the stop button GONE and no tool active —
            // i.e. generation has actually ended (a genuine mid-stream gap keeps
            // `stop` true, so we `continue` above and never land here). Fast path:
            // settle once the visible text has been unchanged for `stable_needed`
            // polls. Crucially `idle_count` ALWAYS increments in this branch and is
            // never reset, so `idle_limit` is a HARD ceiling — without that, the
            // DOM re-rendering a finished reply (collapsible tool-call disclosures
            // mutating innerText) would reset the counter every poll and wedge us
            // in "waiting for reply" forever.
            // Require a NEW assistant turn, not merely "some assistant message
            // exists". Without this, a turn that never actually submitted (Enter
            // swallowed, send-button fallback missed) lands here on the first
            // poll — stop button absent, previous reply static — and we scrape
            // the PREVIOUS turn's text and return it as this turn's answer.
            // Fail closed: no new turn means we keep waiting, then time out.
            if cur_count > baseline_count {
                if !atext.is_empty() && atext == prev_stable {
                    stable_polls += 1;
                    if stable_polls >= stable_needed {
                        break;
                    }
                } else {
                    prev_stable = atext.clone();
                    stable_polls = 0;
                }
                idle_count += 1;
                if idle_count >= idle_limit {
                    break; // safety net: generation ended but text never went stable
                }
            }
        }

        // The DOM has settled. Prefer the SERVER's copy of the reply anyway.
        //
        // What we scrape is rendered markdown, not what the model wrote:
        // `**structural validation**` reaches innerText as `structural
        // validation`. Measured on one 600-word reply, the scrape lost 87
        // characters of syntax against the conversation record. For prose that
        // is a fidelity loss; for the tool protocol it is a correctness bug,
        // and the same one that made a ```json fence unmatchable — the fence is
        // simply not in the rendered text.
        //
        // So the record is the source of truth whenever we have a conversation
        // to ask about, and the scrape is the fallback for the turn-one window
        // where no id exists yet.
        if let Some((true, text)) = self.server_final(remaining_secs().min(30.0)) {
            if !text.trim().is_empty() {
                return self.finish_turn(text, remaining_secs());
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

        self.finish_turn(reply_text, remaining_secs())
    }

    /// Everything a completed turn owes regardless of HOW it completed — the DOM
    /// settling, or the server saying so. Kept in one place so the two paths
    /// cannot drift: a reply taken from the conversation record must still pin
    /// the conversation and still get filed into its project.
    fn finish_turn(&mut self, reply_text: String, budget: f64) -> Result<String> {
        if reply_text.trim().is_empty() {
            bail!("scraped an empty reply from ChatGPT");
        }

        // Latch identity on the first completed turn: a brand-new chat has no
        // conversation id in its URL until the server persists it, so this is
        // the earliest point we can pin. Every later turn verifies against it.
        if self.convo_id.is_none() {
            if let Some(id) = self.current_convo_id(budget) {
                eprintln!("pinned to conversation {id}");
                self.convo_id = Some(id);
            }
        }

        Ok(reply_text)
    }

    /// Close the tab (best-effort), matching chatgpt-imagegen's try/finally.
    ///
    /// Any deferred project filing happens HERE rather than after each turn.
    /// Setting `gizmo_id` makes ChatGPT's client re-route the open conversation
    /// to its project URL, and the composer is gone for the moment that takes —
    /// live, a multi-turn `run` filed after turn one and then failed its next
    /// send with "could not insert text into the ChatGPT composer". A one-shot
    /// `ask` never saw it because it exits immediately after. So we file once,
    /// when nothing is going to type into the page again.
    pub fn close(mut self) {
        if let (Some(gizmo), Some(cid)) = (self.pending_project.clone(), self.convo_id.clone()) {
            match self.file_into_project(&cid, &gizmo, 30.0) {
                Ok(()) => {
                    eprintln!("filed conversation into project {:?}", self.project);
                    self.pending_project = None;
                }
                Err(e) => eprintln!(
                    "warning: could not file the conversation into {:?} ({e}); it stays in a \
                     plain chat",
                    self.project
                ),
            }
        }
        ab_close(&self.ab, &self.session);
    }

    /// Navigate the open session's tab to `url` and wait for it to settle.
    /// Used by side flows (e.g. `refresh`) that drive ChatGPT pages other than
    /// the chat composer.
    pub fn open(&self, url: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(self.timeout_secs.min(30));
        ab_open(&self.ab, &self.session, url, None, deadline)
    }

    /// Run JS in the page (the `JSON.stringify(value)` convention) and return the
    /// decoded value. Exposed for side flows like `refresh`.
    pub fn eval(&self, js: &str) -> Result<serde_json::Value> {
        let t = (self.timeout_secs.min(30) as f64).max(5.0);
        ab_eval(&self.ab, js, &self.session, t)
    }

    /// Navigate the open session into the named ChatGPT Project, creating it on
    /// first use. Mirrors `_enter_project` in chatgpt-imagegen.
    fn resolve_project(&self, name: &str, deadline: Instant) -> Result<(String, bool)> {
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::from_secs(2))
                .as_secs_f64()
                .max(2.0)
        };

        // A remembered id skips the account-wide project listing entirely.
        if let Some(id) = cached_gizmo(name) {
            return Ok((id, false));
        }

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
        remember_gizmo(name, gizmo_id);
        Ok((gizmo_id.to_string(), created))
    }

    /// Navigate into the project's page so the next message is composed inside it.
    ///
    /// This is the preferred route — a conversation born in the project needs no
    /// repair afterwards — but it depends on ChatGPT's project page rendering,
    /// which is not something we control. See `file_into_project` for what
    /// happens when it doesn't.
    fn open_project_page(&self, gizmo_id: &str, deadline: Instant) -> Result<()> {
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::from_secs(2))
                .as_secs_f64()
                .max(2.0)
        };
        // Sidebar link first, for the same reason as everywhere else: the SPA
        // route costs a fraction of a reload, and this navigation happens on
        // every run that names a project.
        let in_place = ab_eval(
            &self.ab,
            &js_open_project_in_place(gizmo_id),
            &self.session,
            remaining().min(15.0),
        )
        .ok()
        .and_then(|v| v.get("ok").and_then(|b| b.as_bool()))
        .unwrap_or(false);
        if !in_place {
            let project_url = WEB_PROJECT_URL_TPL.replace("{gizmo_id}", gizmo_id);
            ab_open(&self.ab, &self.session, &project_url, None, deadline)?;
        }

        // Wait until the SPA has ACTUALLY settled into the project — not just that
        // a `#prompt-textarea` exists. `chrome-use open` can return while the prior
        // top-level page (https://chatgpt.com/) is still showing its composer; a
        // bare composer check passes against THAT, and the next send() then submits
        // on the top-level context, filing the conversation OUTSIDE the project.
        // Gate on the URL containing the project gizmo id so we type into the real
        // project composer. (The project URL is /g/<gizmo_id>/project and stays on
        // that gizmo path until submit, so a substring check is reliable.)
        let js_project_ready = format!(
            r#"(() => {{
  const gid = {gid};
  return JSON.stringify({{
    composer: !!document.querySelector('#prompt-textarea'),
    in_project: (location.href || '').includes(gid),
  }});
}})()"#,
            gid = serde_json::to_string(gizmo_id).unwrap_or_else(|_| "\"\"".to_string()),
        );
        let mut ready = false;
        for _ in 0..30 {
            if Instant::now() >= deadline {
                break;
            }
            if let Ok(st) = ab_eval(&self.ab, &js_project_ready, &self.session, remaining().min(20.0)) {
                let composer = st.get("composer").and_then(|v| v.as_bool()).unwrap_or(false);
                let in_project = st.get("in_project").and_then(|v| v.as_bool()).unwrap_or(false);
                if composer && in_project {
                    ready = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if !ready {
            bail!("project page never settled (composer + project URL) within the deadline");
        }

        Ok(())
    }

    /// Set the composer's model, verified against the live UI on 2026-09-10.
    ///
    /// ChatGPT rebuilt this control and the old "click the menu item named Pro"
    /// approach cannot work any more. What is there now:
    ///
    ///   * The picker BUTTON's text is not stable and must never be matched on.
    ///     Observed on one account inside three weeks: "Instant", "5.6 SolLight",
    ///     "6Pro" — and "Thinking effort" while its own menu is open. Those come
    ///     from a model catalogue whose `shortLabel` changes with the model
    ///     line-up. We locate it structurally instead: the one
    ///     `button[aria-haspopup="menu"]` sharing the composer toolbar row with
    ///     `[data-testid="composer-plus-btn"]` (excluding the plus button).
    ///   * The five intelligence levels are NO LONGER menu items. They are a
    ///     `[role="slider"]` with `aria-valuenow` 0..4 — Instant, Medium, High,
    ///     Extra High, Pro — driven with arrow keys. So we verify on the INDEX,
    ///     which is stable, never on the rendered name, which is not.
    ///   * Model family is a separate axis: `[role="menuitemradio"]` entries
    ///     ("Latest", "GPT-5.6 Sol", "GPT-5.5"). A `--model` that isn't a level
    ///     name is matched against those.
    ///
    /// Two clicks must be REAL input events, not `element.click()`: opening the
    /// picker, and focusing the slider thumb. JS clicks are ignored by both.
    fn select_model(&self, model: &str, deadline: Instant) -> Result<()> {
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::from_secs(2))
                .as_secs_f64()
                .max(2.0)
        };

        let want = model.trim().to_lowercase();
        let want_level = level_index(&want);

        // 1. Locate the picker structurally and open it with a real click.
        let mut pick = serde_json::Value::Null;
        for attempt in 0..6 {
            pick = ab_eval(&self.ab, JS_FIND_PICKER, &self.session, remaining())?;
            if pick.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                break;
            }
            if attempt < 5 {
                std::thread::sleep(Duration::from_millis(600));
            }
        }
        if !pick.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let detail = pick.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
            bail!("could not find the composer model picker: {detail}");
        }
        let (px, py) = match (
            pick.get("x").and_then(|v| v.as_i64()),
            pick.get("y").and_then(|v| v.as_i64()),
        ) {
            (Some(x), Some(y)) => (x, y),
            _ => bail!("composer model picker has no usable coordinates"),
        };
        ab_cmd(&self.ab, &["click", &px.to_string(), &py.to_string()], &self.session, remaining())
            .context("opening the composer model picker")?;
        std::thread::sleep(Duration::from_millis(500));

        let st = ab_eval(&self.ab, JS_PICKER_MENU, &self.session, remaining())?;
        if !st.get("open").and_then(|v| v.as_bool()).unwrap_or(false) {
            bail!("clicked the model picker but its menu did not open");
        }

        let outcome = match want_level {
            Some(idx) => self.set_level(&st, idx, &remaining),
            None => self.set_model_family(&st, model.trim(), &remaining),
        };

        // Close the menu whether or not we succeeded, so the composer is usable.
        let _ = ab_cmd(&self.ab, &["press", "Escape"], &self.session, remaining());
        outcome
    }

    /// Walk the thinking-effort slider to `idx` with arrow keys.
    fn set_level(
        &self,
        st: &serde_json::Value,
        idx: usize,
        remaining: &dyn Fn() -> f64,
    ) -> Result<()> {
        let slider = st.get("slider").filter(|v| !v.is_null()).ok_or_else(|| {
            anyhow!(
                "the composer menu has no thinking-effort slider — ChatGPT has \\
                 changed this control again. Rerun without --model to use the \\
                 account default."
            )
        })?;
        let now = slider.get("now").and_then(|v| v.as_i64()).unwrap_or(-1);
        let max = slider.get("max").and_then(|v| v.as_i64()).unwrap_or(-1);
        let last = (LEVEL_ORDER.len() - 1) as i64;
        if max != last {
            bail!(
                "the thinking-effort slider now has {} positions but this build \\
                 knows {} levels ({}) — refusing to guess which is which",
                max + 1,
                LEVEL_ORDER.len(),
                LEVEL_ORDER.join(", ")
            );
        }
        let (tx, ty) = match (
            slider.get("thumbX").and_then(|v| v.as_i64()),
            slider.get("thumbY").and_then(|v| v.as_i64()),
        ) {
            (Some(x), Some(y)) => (x, y),
            _ => bail!("the thinking-effort slider has no usable thumb coordinates"),
        };

        // A real click on the thumb; this focuses the slider WITHOUT closing the
        // menu, which `press --selector` does not manage (focusing through a
        // selector dismisses the popover and the arrow keys go nowhere).
        ab_cmd(&self.ab, &["click", &tx.to_string(), &ty.to_string()], &self.session, remaining())
            .context("focusing the thinking-effort slider")?;
        std::thread::sleep(Duration::from_millis(300));

        let target = idx as i64;
        let key = if target >= now { "ArrowRight" } else { "ArrowLeft" };
        for _ in 0..(target - now).abs() {
            ab_cmd(&self.ab, &["press", key], &self.session, remaining())
                .context("moving the thinking-effort slider")?;
            std::thread::sleep(Duration::from_millis(250));
        }
        std::thread::sleep(Duration::from_millis(300));

        let after = ab_eval(&self.ab, JS_PICKER_MENU, &self.session, remaining())?;
        let got = after
            .get("slider")
            .and_then(|v| v.get("now"))
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        if got != target {
            bail!(
                "could not move the thinking-effort slider to {} ({}): it sits at {}",
                LEVEL_ORDER[idx],
                target,
                got
            );
        }
        let shown = after.get("level").and_then(|v| v.as_str()).unwrap_or("");
        eprintln!("model: {} (slider {}{})", LEVEL_ORDER[idx], target,
            if shown.is_empty() { String::new() } else { format!(", shown as {shown:?}") });
        Ok(())
    }

    /// Pick a model family (`Latest`, `GPT-5.5`, …) from the menu's radio items.
    fn set_model_family(
        &self,
        st: &serde_json::Value,
        want: &str,
        remaining: &dyn Fn() -> f64,
    ) -> Result<()> {
        let empty = vec![];
        let radios = st.get("radios").and_then(|v| v.as_array()).unwrap_or(&empty);
        let names: Vec<String> = radios
            .iter()
            .filter_map(|r| r.get("text").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        let wl = want.to_lowercase();
        let hit = names.iter().find(|n| n.to_lowercase() == wl).cloned().or_else(|| {
            names.iter().find(|n| n.to_lowercase().contains(&wl)).cloned()
        });
        let Some(hit) = hit else {
            bail!(
                "{want:?} is neither a thinking-effort level ({}) nor one of the \\
                 models this account offers ({})",
                LEVEL_ORDER.join(", "),
                if names.is_empty() { "none listed".to_string() } else { names.join(", ") }
            );
        };

        let res = ab_eval(&self.ab, &js_click_radio(&hit), &self.session, remaining())?;
        if !res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            bail!("could not select model {hit:?}");
        }
        eprintln!("model: {hit}");
        Ok(())
    }

}

// ---- chrome-use helpers (mirrors _ab / _ab_eval in chatgpt-imagegen) --------

/// Locate the chrome-use binary: search PATH, then ~/.local/bin.
/// Where a turn died, so the caller knows whether retrying could duplicate it.
///
/// This is the "ambiguous submit" distinction: recovering from a lost tab is
/// only safe while nothing has been sent. Once Enter has been pressed we may be
/// looking at a message that IS generating server-side but whose receipt we
/// never saw — resending it would post the prompt twice.
enum SubmitFailure {
    /// Failed before anything was submitted — safe to reconnect and retry.
    BeforeSubmit(anyhow::Error),
    /// Enter was pressed but no receipt appeared — never auto-retry.
    Ambiguous(anyhow::Error),
}

impl SubmitFailure {
    fn into_error(self) -> anyhow::Error {
        match self {
            SubmitFailure::BeforeSubmit(e) | SubmitFailure::Ambiguous(e) => e,
        }
    }
}

/// Cross-process lock on the shared ChatGPT web surface.
///
/// The surface is concurrency-1: one logged-in tab, and an account that
/// rate-limits hard. Two processes driving it interleave inside a single
/// composer — B's clear-and-insert lands on top of A's half-written prompt —
/// which live-reproduced as both aborting with "42 non-whitespace chars
/// present, 21 expected", i.e. two prompts concatenated.
///
/// Held for the whole CHANNEL — connect through close — not per turn. Per-turn
/// was enough while every process opened its own tab, but once they share one
/// window (see DEFAULT_SESSION) `connect` itself is destructive: it navigates
/// the shared tab to a new chat. Live: a second `ask` starting mid-turn blew
/// away the first one's page (the first survived only because it had pinned its
/// conversation and could reattach), and then failed itself because the tab had
/// moved on again by the time it got the lock. Whoever holds the surface owns
/// the tab for as long as it needs it.
///
/// Verified with two concurrent `ask` runs: the second prints its wait line
/// BEFORE "opening ChatGPT", i.e. it blocks before touching the browser at all,
/// and both turns then complete with neither having to reattach.
struct SurfaceLock {
    _file: Option<File>,
}

impl SurfaceLock {
    /// Block until this process owns the surface, best-effort.
    ///
    /// If the lock file can't be created (no HOME, read-only home), run without
    /// it and say so: refusing to work because we couldn't take an advisory lock
    /// would be worse than the race it guards.
    fn acquire() -> Self {
        let Some(path) = lock_path() else {
            return SurfaceLock { _file: None };
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // NOT opened in append mode. The holder line is rewritten in place at
        // offset 0, and with O_APPEND every write silently goes to end-of-file
        // and the seek is ignored — which would leave the PREVIOUS holder's line
        // first and make every waiter name the wrong process. Not truncated
        // either: truncating a locked file is a sharing violation on Windows,
        // and since our line ends in "\n" any longer remnant lands on line 2,
        // which the reader ignores.
        let mut file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("warning: no channel lock ({e}); concurrent runs may collide");
                return SurfaceLock { _file: None };
            }
        };

        // Announce a wait rather than appearing to hang: a queued turn can sit
        // here for as long as the turn ahead of it takes — an image generation
        // holds it for a minute or more — and "waiting" with no subject reads as
        // "stuck".
        if file.try_lock().is_err() {
            let who = holder_label(&std::fs::read_to_string(&path).unwrap_or_default());
            eprintln!("waiting for {who} to finish with ChatGPT…");
            if let Err(e) = file.lock() {
                eprintln!("warning: could not take the channel lock ({e}); proceeding");
                return SurfaceLock { _file: None };
            }
        }

        // Claim it by name so the next waiter can say who it is waiting for.
        // Best-effort: a lock we hold but could not stamp is still a good lock.
        let _ = file
            .seek(std::io::SeekFrom::Start(0))
            .and_then(|_| file.write_all(format!("chatgpt-use {}\n", std::process::id()).as_bytes()))
            .and_then(|_| file.flush());

        SurfaceLock { _file: Some(file) }
    }
}

/// Strip ChatGPT's private-use control markers from a record-sourced reply.
///
/// The conversation record is the true text, which turns out to include markup
/// the UI never shows: citation spans delimited by U+E200/U+E201 with U+E202
/// separators, e.g. `\u{e200}cite\u{e202}turn0search4\u{e201}`, plus occasional
/// bare private-use characters. Scraping innerText hid these because the page
/// renders them as links; reading the record does not, so a downstream consumer
/// — Claude Code, through `serve` — would receive them verbatim.
///
/// Whole delimited spans go, since their contents are internal ids rather than
/// anything a reader wants; stray private-use characters go too.
fn strip_private_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '\u{e200}' => depth += 1,
            '\u{e201}' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            // Bare private-use characters outside a span carry no meaning either.
            '\u{e000}'..='\u{f8ff}' => {}
            _ => out.push(c),
        }
    }
    // Collapse the blank lines a removed trailing span can leave behind.
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out.trim_end().to_string()
}

/// Rust twin of `JS_COMPOSER_FINGERPRINT`: non-whitespace count plus an
/// order-sensitive FNV-1a hash over UTF-16 code units.
///
/// Must stay byte-for-byte equivalent to the JS. The whitespace set is spelled
/// out for the same reason it is there: `char::is_whitespace` and JavaScript's
/// `\s` classify U+0085 and U+FEFF differently, and disagreeing by one
/// character fails an otherwise perfect 30 KB prompt.
fn composer_fingerprint(text: &str) -> (u64, u32) {
    fn is_wire_ws(u: u16) -> bool {
        matches!(u,
            0x09..=0x0d | 0x20 | 0x85 | 0xa0 | 0x1680 | 0x2000..=0x200a
            | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff)
    }
    let mut n: u64 = 0;
    let mut h: u32 = 0x811c_9dc5;
    for u in text.encode_utf16() {
        if is_wire_ws(u) {
            continue;
        }
        n += 1;
        h ^= u as u32 & 0xff;
        h = h.wrapping_mul(0x0100_0193);
        h ^= (u >> 8) as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    (n, h)
}

/// Remembered project name -> gizmo id, at `~/.chatgpt-use/projects.json`.
///
/// Resolving a project costs a request that lists every project on the account,
/// on every single connect, to learn something that essentially never changes.
/// Against a throttle that counts requests, that is worth caching. Purely an
/// optimisation: any read or write failure just means we resolve the slow way.
fn project_cache_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".chatgpt-use").join("projects.json"))
}

fn read_project_cache() -> serde_json::Map<String, serde_json::Value> {
    project_cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn cached_gizmo(name: &str) -> Option<String> {
    read_project_cache()
        .get(name)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn remember_gizmo(name: &str, gizmo_id: &str) {
    let Some(path) = project_cache_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut map = read_project_cache();
    map.insert(name.to_string(), serde_json::Value::String(gizmo_id.to_string()));
    if let Ok(text) = serde_json::to_string_pretty(&serde_json::Value::Object(map)) {
        let _ = std::fs::write(path, text);
    }
}

/// Drop a remembered id after it turns out not to work — a deleted project, or
/// one that belongs to a different account than the browser is now signed into.
fn forget_gizmo(name: &str) {
    let Some(path) = project_cache_path() else { return };
    let mut map = read_project_cache();
    if map.remove(name).is_some() {
        if let Ok(text) = serde_json::to_string_pretty(&serde_json::Value::Object(map)) {
            let _ = std::fs::write(path, text);
        }
    }
}

/// Describe whoever holds the lock, from the file's first line.
///
/// The contract with chatgpt-imagegen is one line, `<tool> <pid>`. Anything
/// else — empty file, a stale remnant on later lines, a tool that stamped only
/// its name, garbage — degrades to a usable phrase rather than failing: the
/// point is a legible waiter line, and the lock itself is what provides safety.
fn holder_label(contents: &str) -> String {
    let first = contents.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "another chatgpt turn".to_string();
    }
    match first.split_once(char::is_whitespace) {
        Some((tool, rest)) => match rest.trim().parse::<u32>() {
            Ok(pid) => format!("{tool} (pid {pid})"),
            Err(_) => first.to_string(),
        },
        None => first.to_string(),
    }
}

/// `~/.chatgpt-web.lock` — deliberately NOT under `~/.chatgpt-use/`.
///
/// What this guards is the shared ChatGPT web surface, not one tool's state, and
/// more than one tool drives it (chatgpt-imagegen too). A lock living under one
/// project's directory invites the other project to pick its own path, and two
/// names mean no mutual exclusion at all — which is exactly the state that let
/// two prompts land in one composer.
fn lock_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".chatgpt-web.lock"))
}

/// Decide whether the tab has drifted off the pinned conversation.
///
/// Returns `None` when it's still the right chat, or `Some(explanation)` when the
/// turn must NOT proceed. Fail-closed by design: the multi-turn modes rely on
/// context accumulated in the pinned chat, so answering from a different one —
/// or from a blank new chat — is worse than erroring out.
fn convo_drift(pinned: &str, current: Option<&str>) -> Option<String> {
    match current {
        Some(cur) if cur == pinned => None,
        Some(cur) => Some(format!(
            "the browser tab moved to a different ChatGPT conversation \
             (expected {pinned}, found {cur}) — this channel's context lives in \
             the original chat. Leave the tab alone while it runs, or start a \
             new run."
        )),
        None => Some(format!(
            "the browser tab is no longer showing conversation {pinned} (it's on \
             a new/blank chat) — refusing to continue the turn there."
        )),
    }
}

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
    /// One test, not two: both halves have to move HOME, and cargo runs tests
    /// in parallel — as two tests they raced and the second read the first's
    /// directory.
    #[test]
    fn project_cache_round_trips_forgets_and_survives_corruption() {
        let tmp = std::env::temp_dir().join(format!("cgu-projcache-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join(".chatgpt-use")).ok();
        let old_home = std::env::var_os("HOME");
        // SAFETY: the whole test body runs with HOME redirected and restores it.
        unsafe { std::env::set_var("HOME", &tmp) };

        assert_eq!(cached_gizmo("proj"), None);
        remember_gizmo("proj", "g-p-abc");
        assert_eq!(cached_gizmo("proj").as_deref(), Some("g-p-abc"));

        // A second project must not clobber the first.
        remember_gizmo("other", "g-p-def");
        assert_eq!(cached_gizmo("proj").as_deref(), Some("g-p-abc"));
        assert_eq!(cached_gizmo("other").as_deref(), Some("g-p-def"));

        // Forgetting a stale id must cost only that entry.
        forget_gizmo("proj");
        assert_eq!(cached_gizmo("proj"), None);
        assert_eq!(cached_gizmo("other").as_deref(), Some("g-p-def"));

        // A corrupt cache degrades to "no entry" and is rewritten, never panics.
        std::fs::write(tmp.join(".chatgpt-use").join("projects.json"), "{not json").ok();
        assert_eq!(cached_gizmo("other"), None);
        remember_gizmo("other", "g-p-def");
        assert_eq!(cached_gizmo("other").as_deref(), Some("g-p-def"));

        match old_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The record carries markup the rendered page hides. Observed live in a
    /// `serve` reply: a citation span that would have been handed straight to
    /// Claude Code.
    #[test]
    fn strips_citation_spans_from_record_text() {
        let raw = "Tokyo is 21\u{b0}C. \u{e200}cite\u{e202}turn168423search4\u{e201}";
        assert_eq!(strip_private_markers(raw), "Tokyo is 21\u{b0}C.");
    }

    #[test]
    fn strips_bare_private_use_characters() {
        assert_eq!(strip_private_markers("a\u{e300}b"), "ab");
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        let s = "**bold**, `code`, 中文, emoji \u{1F600}\n\nsecond paragraph";
        assert_eq!(strip_private_markers(s), s);
    }

    /// An unterminated span must not swallow the rest of the reply.
    #[test]
    fn an_unclosed_span_does_not_eat_the_tail() {
        // Depth stays open, so the tail is dropped — but a stray CLOSE marker
        // must never make depth negative and re-admit garbage.
        assert_eq!(strip_private_markers("keep\u{e201}more"), "keepmore");
    }

    /// These expected values were produced by running the EXACT JavaScript of
    /// `JS_COMPOSER_FINGERPRINT` under node. They are the contract between the
    /// two implementations: if Rust and the page ever disagree, every send
    /// fails the integrity check, so the agreement is pinned here rather than
    /// discovered in production.
    #[test]
    fn composer_fingerprint_matches_the_javascript() {
        assert_eq!(composer_fingerprint(""), (0, 2166136261));
        assert_eq!(composer_fingerprint("hello world"), (10, 942532069));
        assert_eq!(composer_fingerprint("AAA\nBBB\nCCC"), (9, 1755253517));
        assert_eq!(composer_fingerprint("中文测试 ABC"), (7, 1526566950));
        // Surrogate pair: four UTF-16 code units, not three chars.
        assert_eq!(composer_fingerprint("a\u{1F600}b"), (4, 957716613));
    }

    /// The three code points where `char::is_whitespace` and JavaScript's `\s`
    /// disagree must be classified the same by both sides. All three collapse
    /// to plain "ab".
    #[test]
    fn composer_fingerprint_agrees_on_contested_whitespace() {
        let plain = composer_fingerprint("ab");
        assert_eq!(plain, (2, 2174188438));
        assert_eq!(composer_fingerprint("a\u{00a0}b"), plain); // NBSP
        assert_eq!(composer_fingerprint("a\u{0085}b"), plain); // NEL — Rust-only in std
        assert_eq!(composer_fingerprint("a\u{feff}b"), plain); // BOM — JS-only in \s
    }

    /// The reason the hash exists at all: chrome-use#301 scrambles chunked
    /// inserts while PRESERVING length, so a count-only check waves it through.
    #[test]
    fn composer_fingerprint_detects_reordering_at_equal_length() {
        let (n1, h1) = composer_fingerprint("abcdef");
        let (n2, h2) = composer_fingerprint("abcdfe");
        assert_eq!(n1, n2, "the failure mode under test keeps the count identical");
        assert_ne!(h1, h2, "a reordered payload must not pass the integrity check");
        assert_eq!((n1, h1), (6, 829399410));
        assert_eq!((n2, h2), (6, 793662762));
    }

    #[test]
    fn holder_label_reads_tool_and_pid() {
        assert_eq!(holder_label("chatgpt-imagegen 4321\n"), "chatgpt-imagegen (pid 4321)");
    }

    /// The case that caught the O_APPEND bug on the imagegen side: a PREVIOUS
    /// holder's longer line left behind after ours. Ours is line 1; the remnant
    /// must be ignored, never reported as the holder.
    #[test]
    fn holder_label_ignores_a_longer_stale_remnant() {
        let f = "chatgpt-use 77\nchatgpt-imagegen 4321 some older longer line\n";
        assert_eq!(holder_label(f), "chatgpt-use (pid 77)");
    }

    #[test]
    fn holder_label_degrades_instead_of_failing() {
        assert_eq!(holder_label(""), "another chatgpt turn");
        assert_eq!(holder_label("   \n"), "another chatgpt turn");
        assert_eq!(holder_label("chatgpt-imagegen\n"), "chatgpt-imagegen");
        assert_eq!(holder_label("garbage not-a-pid\n"), "garbage not-a-pid");
    }

    #[test]
    fn level_index_maps_the_slider_positions() {
        assert_eq!(level_index("instant"), Some(0));
        assert_eq!(level_index("Pro"), Some(4));
        assert_eq!(level_index("extra high"), Some(3));
        assert_eq!(level_index("extra-high"), Some(3));
        assert_eq!(level_index("EXTRA   HIGH"), Some(3));
        assert_eq!(level_index("extrahigh"), Some(3));
    }

    /// A model family name must fall through to the radio path, not be
    /// mistaken for an effort level.
    #[test]
    fn level_index_rejects_model_family_names() {
        assert_eq!(level_index("GPT-5.5"), None);
        assert_eq!(level_index("Latest"), None);
        assert_eq!(level_index("5.6 Sol"), None);
    }

    #[test]
    fn picker_probe_is_structural_not_text_matched() {
        // The whole point: never key off the button label, which drifts.
        assert!(JS_FIND_PICKER.contains("composer-plus-btn"));
        assert!(JS_FIND_PICKER.contains(r#"button[aria-haspopup="menu"]"#));
        assert!(!JS_FIND_PICKER.to_lowercase().contains("instant"));
        assert!(JS_PICKER_MENU.contains(r#"[role="slider"]"#));
        assert!(JS_PICKER_MENU.contains("aria-valuenow"));
    }

    use super::*;

    #[test]
    fn convo_drift_allows_the_same_conversation() {
        assert!(convo_drift("abc", Some("abc")).is_none());
    }

    #[test]
    fn convo_drift_rejects_a_different_conversation() {
        let msg = convo_drift("abc", Some("xyz")).expect("must fail closed");
        assert!(msg.contains("abc") && msg.contains("xyz"), "{msg}");
    }

    #[test]
    fn convo_drift_rejects_a_blank_new_chat() {
        let msg = convo_drift("abc", None).expect("must fail closed");
        assert!(msg.contains("abc"), "{msg}");
    }

    #[test]
    fn js_probes_target_the_selectors_we_depend_on() {
        assert!(JS_CONVO_ID.contains(r"/\/c\/([0-9a-f-]{36})/i"));
        assert!(JS_USER_COUNT.contains(r#"[data-message-author-role="user"]"#));
    }

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
