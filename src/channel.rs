//! The shared engine: a `chrome-use`-driven channel to a logged-in ChatGPT web
//! conversation. Every mode goes through here. Port the proven web-driving
//! practices from chatgpt-imagegen (read its source at
//! /Users/leo/github.com/chatgpt-imagegen/chatgpt-imagegen):
//!   - locate the `chrome-use` binary; pick the browser (relay first, then a
//!     logged-in profile; honor `profile = auto|relay|"Profile N"`)
//!   - open chatgpt.com (optionally inside a ChatGPT Project), wait for the
//!     composer (`js_composer_helper!`: ProseMirror or the Lexical editor)
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

/// A rate-limit error, recorded so every process on this machine backs off
/// (see `throttle`). Use it where the throttle is first seen, not for errors
/// that only restate a hit already recorded.
fn rate_limited(msg: impl Into<String>) -> anyhow::Error {
    crate::throttle::note_rate_limited();
    ChannelError::new(ErrorKind::RateLimited, msg).into()
}

/// Refuse while an account cooldown from an earlier hit is still running.
fn check_cooldown() -> Result<()> {
    match crate::throttle::cooldown_left() {
        Some(left) => {
            Err(ChannelError::new(ErrorKind::RateLimited, crate::throttle::refusal(left)).into())
        }
        None => Ok(()),
    }
}

/// Why a channel operation failed, in the terms a caller has to act on.
///
/// Most failures stay plain `anyhow` errors; only the ones that decide what a
/// caller should do next are typed. Retrieve one with [`channel_error`] —
/// it survives any `.context()` layered on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// No signed-in ChatGPT tab could be opened.
    LoginRequired,
    /// chatgpt.com's "Too many requests" throttle.
    RateLimited,
    /// The chrome-use session is wedged (see `Channel::connect`).
    SessionUnavailable,
    /// The turn never completed and the page is showing a dialog we do not
    /// recognise — a plan or usage-limit notice, say. Its text is in the
    /// message; we do not guess what it means.
    PageBlocked,
    /// Failed before the prompt reached ChatGPT. Safe to retry.
    NotSubmitted,
    /// Enter was pressed but no receipt appeared. The prompt may be on the
    /// server; resending it could post it twice.
    SubmitUnknown,
    /// Another run holds the ChatGPT window and `--busy fail` was asked for.
    Busy,
    /// The `--request-id` already names a request that may have reached
    /// ChatGPT; it is not sent again.
    Duplicate,
    /// Cancelled, and confirmed: either before anything was sent, or the
    /// conversation record shows the reply stopped (`finish_details`
    /// "interrupted").
    Cancelled,
    /// Stop was pressed but the record has not confirmed it; the reply may
    /// still be generating.
    CancelRequested,
    /// The prompt was sent but no complete reply arrived.
    Incomplete,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::LoginRequired => "login_required",
            ErrorKind::RateLimited => "rate_limited",
            ErrorKind::SessionUnavailable => "session_unavailable",
            ErrorKind::PageBlocked => "page_blocked",
            ErrorKind::NotSubmitted => "not_submitted",
            ErrorKind::SubmitUnknown => "submit_unknown",
            ErrorKind::Busy => "busy",
            ErrorKind::Duplicate => "duplicate_request",
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::CancelRequested => "cancel_requested",
            ErrorKind::Incomplete => "incomplete",
        }
    }

    /// The caller-facing status this failure maps to. None of them is
    /// "completed": that status belongs to a reply that exists.
    pub fn status(self) -> &'static str {
        match self {
            ErrorKind::LoginRequired
            | ErrorKind::RateLimited
            | ErrorKind::SessionUnavailable
            | ErrorKind::PageBlocked => "unavailable",
            ErrorKind::NotSubmitted => "failed",
            ErrorKind::SubmitUnknown | ErrorKind::Incomplete => "incomplete",
            ErrorKind::Busy => "busy",
            ErrorKind::Duplicate => "duplicate",
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::CancelRequested => "cancel_requested",
        }
    }
}

/// Whether the prompt of the failed turn reached ChatGPT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    No,
    Yes,
    Unknown,
}

impl Submitted {
    pub fn as_str(self) -> &'static str {
        match self {
            Submitted::No => "no",
            Submitted::Yes => "yes",
            Submitted::Unknown => "unknown",
        }
    }
}

#[derive(Debug)]
pub struct ChannelError {
    pub kind: ErrorKind,
    pub submitted: Submitted,
    message: String,
}

impl ChannelError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        ChannelError { kind, submitted: Submitted::No, message: message.into() }
    }

    pub fn with_submitted(mut self, submitted: Submitted) -> Self {
        self.submitted = submitted;
        self
    }
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ChannelError {}

/// The typed cause of a channel failure, if it has one.
pub fn channel_error(e: &anyhow::Error) -> Option<&ChannelError> {
    e.downcast_ref::<ChannelError>()
        .or_else(|| e.chain().find_map(|c| c.downcast_ref::<ChannelError>()))
}

/// Type a failed turn by how far it got. A typed error keeps its kind but
/// takes its `submitted` from the turn, except `Unknown`, which only the
/// submit step can know and nothing later may downgrade.
fn classify(mut e: anyhow::Error, submitted: bool) -> anyhow::Error {
    let phase = if submitted { Submitted::Yes } else { Submitted::No };
    if let Some(ce) = e.downcast_mut::<ChannelError>() {
        if ce.submitted != Submitted::Unknown {
            ce.submitted = phase;
        }
        return e;
    }
    let (kind, label) = if submitted {
        (ErrorKind::Incomplete, "the prompt was sent but the reply did not complete")
    } else {
        (ErrorKind::NotSubmitted, "the prompt was not sent")
    };
    e.context(ChannelError::new(kind, label).with_submitted(phase))
}

/// Set by SIGTERM / SIGINT once `install_cancel_handler` has run. Every wait
/// in a turn watches it, so a cancel is honoured within about a second.
static CANCEL: std::sync::OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> =
    std::sync::OnceLock::new();

/// Turn SIGTERM and SIGINT into a graceful cancel of the current request: stop
/// its reply and confirm it, instead of dying with the generation still
/// running. A second signal exits at once. Installed only by `ask
/// --request-id`, so every other command keeps the default behaviour.
pub fn install_cancel_handler() {
    #[cfg(unix)]
    {
        let flag = CANCEL
            .get_or_init(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .clone();
        for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
            // Order matters: the conditional shutdown sees the flag the first
            // signal is about to set, so only a SECOND signal exits.
            let _ = signal_hook::flag::register_conditional_shutdown(sig, 130, flag.clone());
            let _ = signal_hook::flag::register(sig, flag.clone());
        }
    }
}

thread_local! {
    /// The cancel flag of the task running on this thread, if it has one.
    static TASK_CANCEL: std::cell::RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with `flag` as this thread's cancel signal, beside the process-wide
/// one. A long-lived server (`agent-mcp`) gives each request its own flag, so
/// cancelling one stops only that request, and nothing carries over to the
/// next: the flag is dropped from the thread when `f` returns or panics.
pub fn with_cancel_flag<T>(
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    f: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TASK_CANCEL.with(|c| *c.borrow_mut() = None);
        }
    }
    TASK_CANCEL.with(|c| *c.borrow_mut() = Some(flag));
    let _reset = Reset;
    f()
}

fn cancel_requested() -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    CANCEL.get().is_some_and(|f| f.load(SeqCst))
        || TASK_CANCEL.with(|c| c.borrow().as_ref().is_some_and(|f| f.load(SeqCst)))
}

/// The page's user turns at one moment: how many are rendered, and the stable
/// identities it exposes (see `js_turn_helpers!`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct UserTurns {
    count: u64,
    /// Identity of each rendered user turn.
    ids: Vec<String>,
    /// Every turn identity the page still holds, including virtualized turns
    /// whose contents are not rendered.
    known: Vec<String>,
}

impl UserTurns {
    fn from_json(v: &serde_json::Value) -> Option<Self> {
        let strs = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        Some(UserTurns {
            count: v.get("count")?.as_u64()?,
            ids: strs("ids"),
            known: strs("known"),
        })
    }
}

/// Did a user turn appear in `now` that was not on the page at `base`?
///
/// This is the submit receipt. When the page exposes turn identities the answer
/// is "is some user turn's identity new", which a virtualized chat cannot fool:
/// an old turn scrolling back into view keeps its identity, and one scrolling
/// out does not hide a new one, whereas both move a plain count. If any
/// rendered turn lacks an identity (an older page), it falls back to the count
/// rising.
fn user_turn_rose(base: &UserTurns, now: &UserTurns) -> bool {
    let identified = |t: &UserTurns| t.ids.len() as u64 == t.count;
    if !identified(base) || !identified(now) {
        return now.count > base.count;
    }
    let seen: std::collections::HashSet<&str> =
        base.ids.iter().chain(&base.known).map(String::as_str).collect();
    now.ids.iter().any(|id| !seen.contains(id.as_str()))
}

/// Sleep that wakes early for a cancel.
fn nap(d: Duration) {
    let end = Instant::now() + d;
    while !cancel_requested() {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(200)));
    }
}

/// JS regex literal for the "Too many requests" dialog, in each UI language
/// ChatGPT has been seen to render it in. An English-only test read a throttled
/// zh-CN page as healthy and kept polling into the throttle. Phrases taken from
/// miuuyy/codex-chatgpt-web's dialog matcher.
macro_rules! js_rate_limit_re {
    () => {
        r"/too many requests|requests too quickly|太多请求|太多要求|过于频繁|過於頻繁|リクエストが多すぎます|リクエストの頻度が高すぎます|요청이 너무 많습니다|너무 많은 요청|요청을 너무 빠르게/i"
    };
}

/// JS helpers that read conversation turns in both of ChatGPT's renderers.
///
/// The older one puts `data-message-author-role` on each message. The newer one
/// groups a user message and its reply under one `[data-turn-key]` element,
/// marking the parts `[data-user-message-bubble]` and
/// `[data-conversation-role="assistant"]`, and need not carry the author-role
/// attribute at all. Counting author roles alone would read that page as having
/// no turns, so no submit could ever be confirmed. Role elements inside a group
/// are excluded so a page that has both markings counts each turn once.
///
/// `__cguUserIds` collects stable turn identities for [`user_turn_rose`]: the
/// `data-turn-id` of each user turn, plus every `data-turn-id-container`. Long
/// chats are virtualized; an off-screen turn loses its contents but keeps its
/// container, so remounting it later must not look like a new turn.
macro_rules! js_turn_helpers {
    () => {
        r#"
  const __cguOuter = (els) => els.filter(e => !els.some(o => o !== e && o.contains(e)));
  const __cguUnits = (role, group) => __cguOuter([...document.querySelectorAll(
    `[data-message-author-role="${role}"], [data-content-search-unit-key$=":${role}"], ` +
    `[data-chatgpt-search-unit-key$=":${role}"], [data-turn-key]:has(${group}, ` +
    `[data-content-search-unit-key$=":${role}"], [data-chatgpt-search-unit-key$=":${role}"])`)]);
  const __cguUsers = () => __cguUnits('user', '[data-user-message-bubble]');
  const __cguAssistants = () => __cguUnits('assistant',
    '[data-conversation-role="assistant"], [data-chatgpt-agent-turn-start]');
  const __cguParts = (el) => {
    if (!el.matches('[data-turn-key]')) return [el];
    const own = __cguOuter([...el.querySelectorAll('[data-conversation-role="assistant"], ' +
      '[data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], ' +
      '[data-chatgpt-search-unit-key$=":assistant"]')]);
    return own.length ? own : __cguOuter([...el.querySelectorAll('.markdown')]);
  };
  const __cguText = (el) => el ? __cguParts(el)
    .map(e => (e.innerText || e.textContent || '').trim()).filter(Boolean).join('\n\n') : '';
  const __cguReplyText = () => {
    const a = __cguAssistants();
    const last = a[a.length - 1];
    if (!last) return '';
    const turn = (e) => { const t = e.closest('[data-turn-id]'); return t && t.getAttribute('data-turn-id'); };
    const users = __cguUsers();
    const lastUser = users[users.length - 1];
    if (!lastUser) return __cguText(last);
    // Only what follows the latest question is its reply. A grouped turn is
    // both its user and its assistant unit, so it counts as following itself.
    const after = a.filter(e => e === lastUser ||
      !(e.compareDocumentPosition(lastUser) & Node.DOCUMENT_POSITION_FOLLOWING));
    const id = turn(last);
    const tail = id ? after.filter(e => turn(e) === id) : after;
    return tail.map(__cguText).filter(Boolean).join('\n\n');
  };
  const __cguUserIds = () => {
    const ids = [];
    const known = [...document.querySelectorAll('[data-turn-id-container]')]
      .map(e => e.getAttribute('data-turn-id-container')).filter(Boolean);
    for (const u of __cguUsers()) {
      const key = u.getAttribute('data-turn-key');
      if (key) { ids.push('group:' + key); continue; }
      const unit = u.getAttribute('data-content-search-unit-key') || u.getAttribute('data-chatgpt-search-unit-key');
      if (unit) { ids.push('unit:' + unit); continue; }
      const t = u.closest('[data-turn-id]');
      const id = t && t.getAttribute('data-turn-id');
      if (id) ids.push(id);
    }
    for (const g of document.querySelectorAll('[data-turn-key]')) known.push('group:' + g.getAttribute('data-turn-key'));
    return {ids, known};
  };
"#
    };
}

/// JS helper `__cguComposer()`: the one composer the user would type into, or
/// null.
///
/// ChatGPT's editor has two forms in the wild: the ProseMirror box with id
/// `#prompt-textarea`, and the newer Lexical rich editor, which need not carry
/// that id at all (selectors as recorded by miuuyy/codex-chatgpt-web
/// `chatgpt-session.ts` and totec448-spec/chat-on-steroids `chatgpt-dom.js`,
/// both early October 2026). Looking only for the id read a page with the new
/// editor as having no composer, and every run failed before sending.
///
/// The app also keeps earlier pages mounted but hidden
/// (`[data-app-shell-page-surface]` with `display: none`), each with its own
/// editor, so the visible one is chosen, never the first match. Nested matches
/// resolve to the innermost editable node. More than one visible candidate is
/// ambiguous and reads as none: typing into the wrong box is worse than failing.
///
/// The chosen node is stamped `data-cgu-composer`, so a real-input click can
/// target it by selector (`[data-cgu-composer]`).
macro_rules! js_composer_helper {
    () => {
        r#"
  const __cguComposer = () => {
    const hidden = (n) => {
      for (let p = n.closest('[data-app-shell-page-surface]'); p;
           p = p.parentElement && p.parentElement.closest('[data-app-shell-page-surface]')) {
        if (getComputedStyle(p).display === 'none') return true;
      }
      return !!n.closest('[hidden], [aria-hidden="true"], [inert]');
    };
    const all = [...document.querySelectorAll(
      '#prompt-textarea, [data-testid="prompt-textarea"], ' +
      '[contenteditable="true"][data-lexical-editor="true"], ' +
      'form[data-chatgpt-composer] [contenteditable="true"][role="textbox"], ' +
      'form [data-composer-markdown][contenteditable="true"][role="textbox"]')]
      .filter(n => !hidden(n) && !n.closest('[data-turn-key], [data-message-author-role], .markdown'));
    const live = all.filter(n => !all.some(o => o !== n && n.contains(o)));
    for (const n of document.querySelectorAll('[data-cgu-composer]')) {
      if (!(live.length === 1 && live[0] === n)) n.removeAttribute('data-cgu-composer');
    }
    if (live.length !== 1) return null;
    live[0].setAttribute('data-cgu-composer', '1');
    return live[0];
  };
"#
    };
}

// JS: press the stop button, if the page is generating.
const JS_CLICK_STOP: &str = r#"(() => {
  const b = document.querySelector('button[data-testid="stop-button"], button[data-testid="composer-stop-button"], ' +
    'form button[type="button"][aria-label="Stop"], button[aria-label="Stop streaming"], button[aria-label="Stop generating"]');
  if (b) b.click();
  return JSON.stringify({clicked: !!b});
})()"#;

// JS: is this the signed-out page? Its login/sign-up buttons, or an auth URL.
// The button test ids are a best guess, not yet checked against a live
// signed-out page. If they are wrong, the error is the safe one: a real
// signed-out page reads as session_unavailable, never a false login_required.
const JS_WANTS_LOGIN: &str = r#"(() => JSON.stringify({login:
  /\/auth\/|auth\.openai\.com/.test(location.href) ||
  !!document.querySelector('[data-testid="login-button"],[data-testid="signup-button"]'),
  path: location.pathname, title: document.title.slice(0, 80)}))()"#;

// JS: text of a visible dialog, if any. Read only once a turn has stalled, to
// report WHAT is blocking it rather than guess (a usage-limit notice, say).
const JS_BLOCKING_DIALOG: &str = r#"(() => {
  const d = [...document.querySelectorAll('[role="dialog"],[role="alertdialog"]')]
    .find(x => x.getClientRects().length > 0);
  return JSON.stringify({text: d ? (d.innerText || '').trim().slice(0, 400) : ''});
})()"#;

// JS: poll composer presence + rate-limit dialog (mirrors _JS_COMPOSER in chatgpt-imagegen).
const JS_COMPOSER: &str = concat!(
    "(() => {",
    js_composer_helper!(),
    r#"
  const dlg = [...document.querySelectorAll('[role="dialog"]')]
    .map(d => d.textContent || '').join(' ');
  return JSON.stringify({
    composer: !!__cguComposer(),
    limited: "#,
    js_rate_limit_re!(),
    r#".test(dlg),
  });
})()"#
);

// JS: poll generation/reply state: stop button present? newest assistant text?
// rate-limited? Mirrors _JS_STATE in chatgpt-imagegen but without image scraping.
const JS_STATE: &str = concat!(
    "(() => {",
    js_turn_helpers!(),
    r#"
  const stop = !!document.querySelector(
    'button[data-testid="stop-button"], button[data-testid="composer-stop-button"], button[aria-label*="Stop" i]'
  );
  const a = __cguAssistants();
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
    .filter(b => !b.closest('[data-message-author-role], [data-conversation-role]')) // exclude in-message buttons
    .some(b => {
      const t = (b.textContent || '').trim();
      return t.length > 0 && t.length <= 24 && ACTIVE.test(t);
    });
  const cm = location.pathname.match(/\/c\/([0-9a-f-]{36})/i);
  return JSON.stringify({
    stop,
    tool_active,
    convo: cm ? cm[1] : "",
    user_count: __cguUsers().length,
    assistant_count: a.length,
    limited: "#,
    js_rate_limit_re!(),
    r#".test(dlg),
    atext: __cguReplyText()
  });
})()"#
);

// JS: scrape the full innerText of the last assistant message.
const JS_LAST_ASSISTANT: &str = concat!(
    "(() => {",
    js_turn_helpers!(),
    r#"
  return JSON.stringify(__cguReplyText());
})()"#
);

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
const JS_ASSISTANT_COUNT: &str = concat!(
    "(() => {",
    js_turn_helpers!(),
    r#"
  return JSON.stringify(__cguAssistants().length);
})()"#
);

// JS: the user turns as a [`UserTurns`] snapshot — count plus identities.
const JS_USER_TURNS: &str = concat!(
    "(() => {",
    js_turn_helpers!(),
    r#"
  const {ids, known} = __cguUserIds();
  return JSON.stringify({count: __cguUsers().length, ids, known});
})()"#
);

// JS: empty the composer, so a leftover fragment from an aborted turn can't be
// prepended to the next message.
const JS_CLEAR_COMPOSER: &str = concat!(
    "(() => {",
    js_composer_helper!(),
    r#"
  const c = __cguComposer();
  if (!c) return JSON.stringify({ok: false});
  c.focus();
  document.execCommand('selectAll');
  document.execCommand('delete');
  return JSON.stringify({ok: true});
})()"#
);

// JS: dismiss a blocking dialog (the rate-limit notice has a "Got it" button).
// Leaving it up keeps the composer unusable even after the throttle lifts.
const JS_DISMISS_DIALOG: &str = concat!(
    r#"(() => {
  const dlg = [...document.querySelectorAll('[role="dialog"]')]
    .find(d => "#,
    js_rate_limit_re!(),
    r#".test(d.textContent || ''));
  if (!dlg) return JSON.stringify({ok: false});
  const btn = [...dlg.querySelectorAll('button')]
    .find(b => /^(got it|ok|dismiss|close|知道了|了解|关闭|關閉|확인|알겠습니다|閉じる)$/i.test((b.textContent || '').trim()));
  if (btn) { btn.click(); return JSON.stringify({ok: true}); }
  return JSON.stringify({ok: false});
})()"#
);

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
const JS_COMPOSER_FINGERPRINT: &str = concat!(
    "(() => {",
    js_composer_helper!(),
    r#"
  const c = __cguComposer();
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
})()"#
);

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
        r#"(() => {{{helper}
  const c = __cguComposer();
  if (!c) return JSON.stringify({{ok: false, error: 'composer not found'}});
  c.focus();
  const ok = document.execCommand('insertText', false, {t});
  return JSON.stringify({{ok}});
}})()"#,
        helper = js_composer_helper!()
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
    // Only messages AFTER the latest user turn belong to this turn: walking past
    // it would find the previous turn's reply and report it as this one, done.
    for (const n of chain) {{
      const m = n.message;
      if (!m || !m.author) continue;
      if (m.author.role === 'user') break;
      if (m.author.role !== 'assistant') continue;
      // A stopped or length-capped turn is closed (end_turn true) but its text
      // is partial; only finish_details tells it from a real finish. Report it
      // even with no text, which is how a turn stopped early looks.
      const fd = (m.metadata && m.metadata.finish_details) || {{}};
      if (fd.type === 'interrupted' || fd.type === 'max_tokens') {{
        return JSON.stringify({{ok: true, done: true, text: textOf(m.content), finish: fd.type,
                                reason: fd.reason || null, async_status: null}});
      }}
      if (m.weight === 0) continue;
      const ct = m.content && m.content.content_type;
      if (ct === 'reasoning_recap' || ct === 'thoughts') continue;
      const t = textOf(m.content);
      if (!t) continue;
      return JSON.stringify({{ok: true, done: m.end_turn === true, text: t,
                              finish: fd.type || null, reason: fd.reason || null,
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

/// `--model current` (or `default`): use whatever the account is set to and
/// leave the picker alone. An escape hatch for when the picker has changed
/// again and a run does not need a particular model.
pub fn keeps_current_model(model: &str) -> bool {
    matches!(model.trim().to_lowercase().as_str(), "current" | "default")
}

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
  // Only the link to the project PAGE. The sidebar also lists conversations
  // inside the project, whose hrefs carry the same gizmo id; clicking one opens
  // that old conversation, and the readiness check (URL contains the gizmo id)
  // would pass on it, so the new prompt would land in the old chat.
  const link = [...document.querySelectorAll('a[href*="/g/"]')]
    .find(a => {{
      const href = a.getAttribute('href') || '';
      return href.includes(want) && /\/project\/?$/.test(href);
    }});
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
// JS: find the composer's model / effort picker button. ChatGPT names it in
// several ways across rollouts (selectors as recorded by
// miuuyy/codex-chatgpt-web `chatgpt-session.ts`, Oct 2026); the first group
// with exactly ONE visible match wins. The old structural rule — the one menu
// button on the composer row beside `composer-plus-btn` — is the fallback.
// Never matched on its label, which changes with the model line-up.
const JS_FIND_PICKER: &str = r#"(() => {
  const visible = (b) => { const r = b.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
  const at = (b, via) => {
    const r = b.getBoundingClientRect();
    return JSON.stringify({ok: true, via, label: (b.textContent || '').trim(),
                           x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2)});
  };
  for (const [via, sel] of [
    ['testid', 'button[data-testid="model-switcher-dropdown-button"][aria-haspopup="menu"]'],
    ['trigger', 'button[data-codex-intelligence-trigger="true"][aria-haspopup="menu"]'],
    ['tone', 'button[aria-haspopup="menu"][data-tone="neutral"]'],
  ]) {
    const hits = [...document.querySelectorAll(sel)].filter(visible);
    if (hits.length === 1) return at(hits[0], via);
  }
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
  return at(hits[0], 'row');
})()"#;

// JS: read the opened picker — the effort slider (index + thumb position) and
// the model-family radios. `level` is the name the page currently shows for the
// slider position; it is for logging only, never for matching. The picker's
// content need not be a [role="menu"] any more: it can be a plain container
// with data-testid="composer-intelligence-picker-content", or a [role="group"].
const JS_PICKER_MENU: &str = r#"(() => {
  const SLIDER = '[data-model-reasoning-effort-slider] [role="slider"], [data-model-picker-power-slider] [role="slider"]';
  let menu = null;
  for (const sel of [
    '[data-testid="composer-intelligence-picker-content"]',
    '[role="menu"]:has([role="menuitemradio"], [data-model-reasoning-effort-slider], [data-model-picker-power-slider])',
    '[role="group"]:has([role="menuitemradio"], [data-model-reasoning-effort-slider], [data-model-picker-power-slider])',
    '[role="menu"]',
  ]) {
    menu = document.querySelector(sel);
    if (menu) break;
  }
  const sl = (menu && (menu.querySelector(SLIDER) || menu.querySelector('[role="slider"]')))
    || document.querySelector(SLIDER) || document.querySelector('[role="slider"]');
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
    /// Fail with `ErrorKind::Busy` instead of queueing behind another run.
    pub busy_fail: bool,
    /// Receipt to update as the turn progresses (`ask --request-id`).
    pub receipt: Option<PathBuf>,
    /// Connect even during an account cooldown (see `throttle`).
    pub ignore_cooldown: bool,
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
    /// Whether the current turn's prompt is known to be on the server; decides
    /// how a failure of that turn is typed (see `classify`).
    submitted: bool,
    /// Receipt kept current as the turn progresses (see `receipt`).
    receipt: Option<PathBuf>,
    /// Exclusive claim on the shared ChatGPT window, released when the channel
    /// is dropped or closed.
    _surface: SurfaceLock,
}

impl Channel {
    /// Connect: find chrome-use, choose a logged-in browser, open ChatGPT (in
    /// the project if set), and wait for the composer. Errors clearly if no
    /// logged-in browser is available or the account is rate-limited.
    pub fn connect(opts: &ChannelOptions) -> Result<Self> {
        // An account still cooling down from a recent throttle gets no new
        // page load: that is exactly the traffic that keeps it throttled.
        if !opts.ignore_cooldown {
            check_cooldown()?;
        }

        // Take the surface BEFORE touching the browser: opening the tab and
        // entering a project already mutate the shared window.
        let surface = SurfaceLock::acquire(opts.busy_fail)?;

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
        // "Sign in" is advice only for a page that actually asked for it. A
        // relay that cannot attach, or a page that never renders, used to end
        // in the same "no logged-in browser" line — sending the user off to
        // fix the one thing that was working.
        let mut saw_login = false;
        let mut last_failure: Option<String> = None;

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

            // Observed twice: opening right after the previous run closed its tab
            // gave a page whose composer never appeared, and an unchanged rerun
            // seconds later worked. So a candidate whose page merely failed to
            // render gets one more try before we move on.
            let mut opened_now = try_open(&ab, &session, WEB_NEW_CHAT_URL, prof.as_deref(), deadline);
            if matches!(opened_now, Ok(false)) {
                let (login, what) = page_probe(&ab, &session);
                if !login {
                    eprintln!("the ChatGPT page never showed its composer ({what}); retrying once");
                    ab_close(&ab, &session);
                    std::thread::sleep(Duration::from_secs(3));
                    opened_now = try_open(&ab, &session, WEB_NEW_CHAT_URL, prof.as_deref(), deadline);
                }
            }
            match opened_now {
                Ok(true) => {
                    eprintln!("using {label}");
                    opened = true;
                    break;
                }
                Ok(false) => {
                    // composer never appeared — try next candidate
                    let (login, what) = page_probe(&ab, &session);
                    if login {
                        saw_login = true;
                    } else {
                        last_failure = Some(format!(
                            "{label}: the ChatGPT page loaded ({what}) but showed no composer. \
                             Most often this Chrome profile is signed out of ChatGPT (the signed-out \
                             page has no composer): open chatgpt.com in it and sign in. If it is \
                             signed in, ChatGPT may have changed its editor and chatgpt-use needs \
                             an update"
                        ));
                    }
                    ab_close(&ab, &session);
                }
                Err(e) => {
                    ab_close(&ab, &session);
                    let msg = e.to_string();
                    if msg.contains("rate-limited") || msg.contains("Too many") {
                        return Err(rate_limited(RATE_LIMIT_MSG));
                    }
                    // A chrome-use session name can go temporarily unusable: a
                    // command that runs too long is judged unresponsive and its
                    // daemon is stopped, after which every command on that NAME
                    // returns the same error for a while. It does clear on its
                    // own — observed recovering roughly an hour later — but none
                    // of `session stop --force`, deleting the lifecycle lock or
                    // upgrading the CLI made it clear on demand.
                    //
                    // Trying the next candidate cannot help, and falling through
                    // to "no logged-in ChatGPT browser available. Sign in to
                    // chatgpt.com" tells the user to fix the one thing that is
                    // not broken. Say what happened and give a way through now.
                    if msg.contains("session unresponsive") || msg.contains("stuck") {
                        return Err(ChannelError::new(ErrorKind::SessionUnavailable, format!(
                            "the chrome-use session {session:?} is wedged — every command on \
                             that name is returning \"session unresponsive\". You are still \
                             signed in; this is not a login problem.\n\n  Use another name \
                             meanwhile:  chatgpt-use <cmd> --session chatgpt-web-2\n\n\
                             It is usually caused by one very long chrome-use command (a large \
                             `keyboard inserttext`, say) being judged unresponsive. The name \
                             frees itself later — about an hour, in the case we measured — so \
                             the original is worth retrying rather than abandoning."
                        ))
                        .into());
                    }
                    // other errors: log and try the next candidate
                    eprintln!("warning: {label} failed: {e}");
                    last_failure = Some(format!("{label}: {e}"));
                }
            }
        }

        if let (false, false, Some(last)) = (opened, saw_login, &last_failure) {
            return Err(ChannelError::new(
                ErrorKind::SessionUnavailable,
                format!(
                    "could not open ChatGPT through chrome-use (tried {} candidate(s)). Last \
                     failure — {last}\n\n  Check:  chrome-use status",
                    candidates.len()
                ),
            )
            .into());
        }
        if !opened {
            return Err(ChannelError::new(
                ErrorKind::LoginRequired,
                format!(
                    "no logged-in ChatGPT browser available (tried {} candidate(s)). \
                     Sign in to chatgpt.com in Chrome.",
                    candidates.len()
                ),
            )
            .into());
        }

        let mut chan = Channel {
            ab,
            session,
            timeout_secs,
            project: opts.project.trim().to_string(),
            convo_id: None,
            pending_project: None,
            submitted: false,
            receipt: opts.receipt.clone(),
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
        if let Some(ref model) = opts.model.as_ref().filter(|m| !keeps_current_model(m)) {
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
        baseline_users: &UserTurns,
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

        // Focus and empty the composer, then insert the message as TEXT. The
        // probe stamps the composer it finds (`js_composer_helper!`), and the
        // real-input click targets that stamp.
        let found = ab_eval(&self.ab, JS_COMPOSER, &self.session, budget)
            .ok()
            .and_then(|v| v.get("composer").and_then(|b| b.as_bool()))
            .unwrap_or(false);
        if !found {
            bail!("no single visible ChatGPT composer to type into");
        }
        ab_cmd(&self.ab, &["click", "[data-cgu-composer]"], &self.session, budget)
            .context("clicking the composer")?;
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
    fn submit(&self, baseline_users: &UserTurns, budget: f64) -> std::result::Result<(), SubmitFailure> {
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
                &["click", r#"button[data-testid="send-button"], form[data-chatgpt-composer] button[type="submit"]"#],
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
    fn server_final(&self, budget: f64) -> Option<ServerTurn> {
        let id = self.convo_id.as_ref()?;
        let res = ab_eval(&self.ab, &js_server_final(id), &self.session, budget).ok()?;
        if !res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return None;
        }
        let field = |k: &str| res.get(k).and_then(|v| v.as_str()).map(str::to_string);
        Some(ServerTurn {
            done: res.get("done").and_then(|v| v.as_bool()).unwrap_or(false),
            text: strip_private_markers(&field("text").unwrap_or_default()),
            finish: field("finish"),
            reason: field("reason"),
        })
    }

    /// The server's verdict on this turn: its reply once it has properly
    /// finished, `None` while it is open or the record is unreadable, and an
    /// error when it closed cut off (see `judge_record`).
    fn server_verdict(&self, budget: f64) -> Result<Option<String>> {
        match self.server_final(budget) {
            Some(turn) => judge_record(turn),
            None => Ok(None),
        }
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

    /// The rendered user turns, or `None` if the page couldn't be read.
    fn user_turns(&self, budget: f64) -> Option<UserTurns> {
        ab_eval(&self.ab, JS_USER_TURNS, &self.session, budget)
            .ok()
            .and_then(|v| UserTurns::from_json(&v))
    }

    /// Poll up to `within` for the user-turn count to exceed `baseline` — i.e.
    /// for positive evidence that our submit was accepted.
    fn await_user_turn(&self, baseline: &UserTurns, within: Duration, budget: f64) -> bool {
        let until = Instant::now() + within;
        loop {
            if self.user_turns(budget).is_some_and(|now| user_turn_rose(baseline, &now)) {
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

    /// The conversation this channel is pinned to, once a turn has latched one.
    pub fn conversation_id(&self) -> Option<&str> {
        self.convo_id.as_deref()
    }

    /// Attach to an existing conversation to read its reply, without typing.
    ///
    /// Opens a plain chat (no project entry, no model change) and uses its tab
    /// only as a signed-in origin for the conversation API: the conversation
    /// itself is never navigated to, typed into or filed, so attaching cannot
    /// disturb it or any other one.
    pub fn attach(opts: &ChannelOptions, convo_id: &str) -> Result<Self> {
        let plain = ChannelOptions {
            profile: opts.profile.clone(),
            session: opts.session.clone(),
            project: String::new(),
            timeout_secs: opts.timeout_secs,
            model: None,
            busy_fail: opts.busy_fail,
            receipt: None,
            ignore_cooldown: opts.ignore_cooldown,
        };
        let mut chan = Channel::connect(&plain)?;
        chan.convo_id = Some(convo_id.to_string());
        chan.submitted = true;
        Ok(chan)
    }

    /// Wait for the pinned conversation's reply to finish, reading only the
    /// server record (`end_turn`). Used to resume a request whose owner went
    /// away; it never sends anything.
    pub fn await_record(&mut self) -> Result<String> {
        let Some(id) = self.convo_id.clone() else {
            bail!("no conversation to wait on");
        };
        let deadline = Instant::now() + Duration::from_secs(self.timeout_secs);
        loop {
            if let Some(text) = self.server_verdict(30.0)? {
                return Ok(text);
            }
            if Instant::now() >= deadline {
                let why = "the reply has not finished, or the record could not be read";
                return Err(ChannelError::new(
                    ErrorKind::Incomplete,
                    format!("{why} after {}s (conversation {id})", self.timeout_secs),
                )
                .with_submitted(Submitted::Yes)
                .into());
            }
            nap(Duration::from_secs(5));
            if cancel_requested() {
                // Stop WAITING; the generation itself is not ours to stop here.
                return Err(ChannelError::new(
                    ErrorKind::Incomplete,
                    format!("stopped waiting for conversation {id}; the reply may still be generating"),
                )
                .with_submitted(Submitted::Yes)
                .into());
            }
        }
    }

    /// Honour a cancel mid-turn: press stop, and report `Cancelled` only once
    /// the conversation record confirms the reply stopped.
    ///
    /// Stop is pressed only while the tab shows OUR pinned conversation — the
    /// stop button acts on whatever page is open, and a cancel must never stop
    /// somebody else's reply. A reply that finished before the stop took
    /// effect is returned as the reply it is.
    fn stop_generation(&mut self, budget: f64) -> Result<String> {
        eprintln!("cancel requested; stopping this conversation's reply");
        let give_up = Instant::now() + Duration::from_secs(20);
        loop {
            let current = self.current_convo_id(10.0);
            let on_ours = match &self.convo_id {
                Some(pinned) => convo_drift(pinned, current.as_deref()).is_none(),
                // Turn one, before ChatGPT has assigned an id: the session's
                // tab is the fresh chat this channel opened.
                None => true,
            };
            if on_ours {
                let _ = ab_eval(&self.ab, JS_CLICK_STOP, &self.session, 10.0);
            }
            if self.convo_id.is_none() && current.is_some() {
                self.convo_id = current;
                self.touch_receipt();
            }
            if let Some(turn) = self.server_final(10.0) {
                if turn.finish.as_deref() == Some("interrupted") {
                    return Err(ChannelError::new(
                        ErrorKind::Cancelled,
                        format!(
                            "cancelled: the conversation record confirms the reply stopped \
                             ({} characters were generated)",
                            turn.text.chars().count()
                        ),
                    )
                    .with_submitted(Submitted::Yes)
                    .into());
                }
                if let Some(text) = judge_record(turn)? {
                    eprintln!("the reply finished before the stop took effect");
                    return self.finish_turn(text, budget);
                }
            }
            if Instant::now() >= give_up {
                return Err(ChannelError::new(
                    ErrorKind::CancelRequested,
                    "pressed stop, but the conversation record has not confirmed it; the reply \
                     may still be generating",
                )
                .with_submitted(Submitted::Yes)
                .into());
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// Cancel the pinned conversation's reply from a channel made by `attach`,
    /// for a request whose owner is gone. A reply that already finished is
    /// returned (nothing to cancel); one still open is reopened so its own
    /// stop button can be pressed.
    pub fn cancel_pinned(&mut self) -> Result<String> {
        let open = match self.server_final(20.0) {
            Some(turn) => !(turn.done || turn.finish.is_some()),
            None => true,
        };
        if open {
            self.reopen_pinned(60.0)?;
        }
        self.stop_generation(60.0)
    }

    /// Record in the receipt, if there is one, what is now known: that the
    /// prompt is on the server, and which conversation it is in.
    fn touch_receipt(&self) {
        let Some(path) = &self.receipt else { return };
        let (submitted, convo) = (self.submitted, self.convo_id.clone());
        crate::receipt::update(path, |r| {
            if submitted {
                r.state = "submitted".into();
                r.submitted = "yes".into();
            }
            if convo.is_some() {
                r.conversation_id = convo;
            }
        });
    }

    /// Text of a visible dialog on the page, if one is up.
    fn blocking_dialog(&self, budget: f64) -> Option<String> {
        let v = ab_eval(&self.ab, JS_BLOCKING_DIALOG, &self.session, budget).ok()?;
        let text = v.get("text")?.as_str()?.trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Send one message with explicit completion tuning (see `SendOptions`).
    ///
    /// A failure carries a [`ChannelError`] saying whether the prompt reached
    /// ChatGPT, so a caller knows if retrying could post it twice.
    pub fn send_with(&mut self, message: &str, sopts: &SendOptions) -> Result<String> {
        self.submitted = false;
        let result = self.send_turn(message, sopts);
        result.map_err(|e| classify(e, self.submitted))
    }

    fn send_turn(&mut self, message: &str, sopts: &SendOptions) -> Result<String> {
        // `serve` holds one channel for hours: a throttle hit on one turn must
        // also hold back the next, not only the next process.
        check_cooldown()?;
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
        // receipt (see `user_turn_rose`).
        let mut baseline_users = self.user_turns(remaining_secs()).unwrap_or_default();

        // Snapshot the current number of assistant messages so we can detect
        // when a NEW one arrives.
        let baseline_count: u64 = ab_eval(&self.ab, JS_ASSISTANT_COUNT, &self.session, remaining_secs())
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // Fill + submit, with ONE reattach-and-retry: the tab can vanish between
        // turns (closed, crashed, browser restarted) and the conversation itself
        // is still on the server, so losing the window shouldn't lose the turn.
        if cancel_requested() {
            return Err(ChannelError::new(
                ErrorKind::Cancelled,
                "cancelled before the prompt was sent",
            )
            .into());
        }

        crate::throttle::note_event(
            crate::throttle::EVENT_SEND,
            serde_json::json!({ "conversation_id": self.convo_id, "chars": message.chars().count() }),
        );
        if let Err(first) = self.fill_and_submit(message, &baseline_users, remaining_secs()) {
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
            let users_now = self.user_turns(remaining_secs()).unwrap_or_default();
            if self.convo_id.is_some() && user_turn_rose(&baseline_users, &users_now) {
                eprintln!("the message had already been submitted; observing that turn");
            } else {
                self.fill_and_submit(message, &users_now, remaining_secs())
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

        // Past this point the prompt is on the server: any failure from here
        // on means an incomplete turn, never "not sent, safe to resend".
        self.submitted = true;
        self.touch_receipt();

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
                if let Some(text) = self.server_verdict(30.0)? {
                    if !text.trim().is_empty() {
                        eprintln!(
                            "the page stopped updating, but the server says this turn finished \
                             — taking the reply from the conversation record"
                        );
                        return self.finish_turn(text, remaining_secs());
                    }
                }
                // Say what is in the way if something visibly is. A plan or
                // usage limit shows up as a dialog whose wording we cannot
                // trigger on demand to match against, so report its text.
                if let Some(text) = self.blocking_dialog(10.0) {
                    return Err(ChannelError::new(
                        ErrorKind::PageBlocked,
                        format!(
                            "the reply did not complete after {}s and ChatGPT is showing a \
                             dialog: {text:?}",
                            self.timeout_secs
                        ),
                    )
                    .into());
                }
                return Err(ChannelError::new(
                    ErrorKind::Incomplete,
                    format!(
                        "timed out after {}s waiting for ChatGPT to complete the reply",
                        self.timeout_secs
                    ),
                )
                .into());
            }
            nap(poll_interval);
            if cancel_requested() {
                return self.stop_generation(remaining_secs());
            }

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
            // hold a submit receipt — the user-turn count rose — and on a short
            // fresh chat that count never legitimately goes DOWN. If it does,
            // we're looking at a different (blank) page.
            //
            // Only before pinning. Once pinned, the URL check below is the
            // authority, and the count is not: a long chat is virtualized, so
            // turns scrolling out of view lower it on a healthy page, and every
            // false "lost" here costs a reattach against the throttle.
            let users_now = read
                .as_ref()
                .ok()
                .and_then(|v| v.get("user_count"))
                .and_then(|v| v.as_u64());
            if self.convo_id.is_none() && users_now.is_some_and(|n| n <= baseline_users.count) {
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
                    self.touch_receipt();
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
                if limited_waits == 1 {
                    // This turn waits it out below; later runs must not pile on.
                    crate::throttle::note_rate_limited();
                }
                let backoff = Duration::from_secs(match limited_waits {
                    1 => 20,
                    2 => 45,
                    _ => 90,
                });
                if Instant::now() + backoff >= deadline {
                    return Err(ChannelError::new(
                        ErrorKind::RateLimited,
                        format!(
                            "{} The prompt was already submitted; check the conversation \
                             or retry in a few minutes.",
                            RATE_LIMIT_MSG
                        ),
                    )
                    .into());
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
                nap(backoff);
                if let Some(text) = self.server_verdict(remaining_secs().min(30.0))? {
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
                if let Some(text) = self.server_verdict(remaining_secs().min(30.0))? {
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
        if let Some(text) = self.server_verdict(remaining_secs().min(30.0))? {
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
                self.touch_receipt();
            }
        }

        Ok(reply_text)
    }

    /// End the channel. Files any deferred project; keeps the tab (see below).
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
        // The tab is deliberately LEFT OPEN. The next run finds it and starts a
        // new chat in place, which costs one backend request; closing it here
        // made every run reopen chatgpt.com from scratch, about 45 requests.
        // Measured: seven runs in a row, all full reloads, none reused, until
        // the account throttle tripped. It is also the one ChatGPT window the
        // shared session is meant to keep.
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
            r#"(() => {{{helper}
  const gid = {gid};
  return JSON.stringify({{
    composer: !!__cguComposer(),
    in_project: (location.href || '').includes(gid),
  }});
}})()"#,
            gid = serde_json::to_string(gizmo_id).unwrap_or_else(|_| "\"\"".to_string()),
            helper = js_composer_helper!(),
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
            let via = pick.get("via").and_then(|v| v.as_str()).unwrap_or("?");
            bail!(
                "clicked the model picker (found by {via}) but its menu did not open; pass \
                 --model current to use the account's current model"
            );
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
            SubmitFailure::BeforeSubmit(e) => e,
            // Whatever the proximate cause, the prompt may be on the server and
            // the caller must not resend it.
            SubmitFailure::Ambiguous(mut e) => {
                if let Some(ce) = e.downcast_mut::<ChannelError>() {
                    ce.submitted = Submitted::Unknown;
                    return e;
                }
                e.context(
                    ChannelError::new(ErrorKind::SubmitUnknown, "the prompt may or may not have been sent")
                        .with_submitted(Submitted::Unknown),
                )
            }
        }
    }
}

/// One read of the conversation record for the current turn.
struct ServerTurn {
    /// `end_turn` on the turn's last assistant message.
    done: bool,
    text: String,
    /// `finish_details.type`: "stop" for a real finish.
    finish: Option<String>,
    reason: Option<String>,
}

/// Judge a record read. A stopped turn is closed like a finished one
/// (`end_turn`, `finished_successfully`, `is_complete` all true) and only
/// `finish_details` sets it apart: observed live, `{"type":"interrupted",
/// "reason":"client_stopped"}` against `{"type":"stop"}`. Its text is partial,
/// so it is an incomplete turn, never a reply. Only KNOWN cut-offs count
/// against a reply; an unrecognised finish type passes as a normal finish, so
/// a model that labels its stop differently keeps working.
fn judge_record(turn: ServerTurn) -> Result<Option<String>> {
    match turn.finish.as_deref() {
        Some(kind @ ("interrupted" | "max_tokens")) => {
            let why = match (kind, turn.reason.as_deref()) {
                ("max_tokens", _) => "hit its length limit and was cut off".to_string(),
                (_, Some(reason)) => format!("was interrupted ({reason})"),
                _ => "was interrupted".to_string(),
            };
            Err(ChannelError::new(
                ErrorKind::Incomplete,
                format!(
                    "ChatGPT's reply {why} before it finished; the {} characters so far are \
                     partial, so they are not returned as a reply",
                    turn.text.chars().count()
                ),
            )
            .with_submitted(Submitted::Yes)
            .into())
        }
        _ if turn.done && !turn.text.trim().is_empty() => Ok(Some(turn.text)),
        _ => Ok(None),
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
    /// If the lock file can't be created (no home directory, read-only home), run without
    /// it and say so: refusing to work because we couldn't take an advisory lock
    /// would be worse than the race it guards.
    fn acquire(fail_fast: bool) -> Result<Self> {
        let Some(path) = lock_path() else {
            return Ok(SurfaceLock { _file: None });
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
                return Ok(SurfaceLock { _file: None });
            }
        };

        // Announce a wait rather than appearing to hang: a queued turn can sit
        // here for as long as the turn ahead of it takes — an image generation
        // holds it for a minute or more — and "waiting" with no subject reads as
        // "stuck".
        if file.try_lock().is_err() {
            let who = holder_label(&std::fs::read_to_string(&path).unwrap_or_default());
            if fail_fast {
                return Err(ChannelError::new(
                    ErrorKind::Busy,
                    format!("{who} is using ChatGPT; not waiting (--busy fail)"),
                )
                .into());
            }
            eprintln!("waiting for {who} to finish with ChatGPT…");
            // Poll rather than block, so a queued request can be cancelled.
            loop {
                match file.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) => {}
                    Err(std::fs::TryLockError::Error(e)) => {
                        eprintln!("warning: could not take the channel lock ({e}); proceeding");
                        return Ok(SurfaceLock { _file: None });
                    }
                }
                if cancel_requested() {
                    return Err(ChannelError::new(
                        ErrorKind::Cancelled,
                        "cancelled while queued for the ChatGPT window; nothing was sent",
                    )
                    .into());
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }

        // Claim it by name so the next waiter can say who it is waiting for.
        // Best-effort: a lock we hold but could not stamp is still a good lock.
        let _ = file
            .seek(std::io::SeekFrom::Start(0))
            .and_then(|_| file.write_all(format!("chatgpt-use {}\n", std::process::id()).as_bytes()))
            .and_then(|_| file.flush());

        Ok(SurfaceLock { _file: Some(file) })
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
        .map(PathBuf::from)
        .or_else(crate::platform::home_dir)
        .map(|h| h.join(".chatgpt-use").join("projects.json"))
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
    // Keep an explicit HOME for compatibility with other tools sharing this
    // lock, but also support native Windows shells where only USERPROFILE (or
    // HOMEDRIVE/HOMEPATH) is set. Missing HOME must not silently disable locking.
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(crate::platform::home_dir)
        .map(|home| home.join(".chatgpt-web.lock"))
}

#[cfg(test)]
#[path = "channel/lock_tests.rs"]
mod lock_tests;

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
    // Prefer a companion binary next to chatgpt-use. This makes portable
    // Windows installs work even when the user's PATH is full or stale.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in AB_BIN_CANDIDATES {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
                #[cfg(windows)]
                {
                    let candidate = dir.join(format!("{name}.exe"));
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    for name in AB_BIN_CANDIDATES {
        if let Some(p) = which_bin(name) {
            return Some(p);
        }
    }
    // Also check ~/.local/bin — common for manual installs on macOS/Linux.
    if let Some(home) = crate::platform::home_dir() {
        for local_bin in [home.join("chrome-tools"), home.join(".local").join("bin")] {
            for name in AB_BIN_CANDIDATES {
                let candidate = local_bin.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
                #[cfg(windows)]
                {
                    let candidate = local_bin.join(format!("{name}.exe"));
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
}

/// Minimal `which`-equivalent: search PATH for a binary name.
fn which_bin(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(crate::platform::path_separator()) {
        if dir.is_empty() {
            continue;
        }
        let p = PathBuf::from(dir).join(name);
        if p.is_file() {
            return Some(p);
        }
        #[cfg(windows)]
        {
            let p = PathBuf::from(dir).join(format!("{name}.exe"));
            if p.is_file() {
                return Some(p);
            }
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
    // A full navigation is the expensive request burst (about 45 for chatgpt.com).
    crate::throttle::note_event(crate::throttle::EVENT_PAGE_LOAD, serde_json::json!({ "url": url }));
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

/// What the open page is, when the composer never appeared: whether it is
/// ChatGPT's signed-out screen (anything unreadable counts as "no": a login
/// claim needs positive evidence), and a short description for the error.
fn page_probe(ab: &PathBuf, session: &str) -> (bool, String) {
    match ab_eval(ab, JS_WANTS_LOGIN, session, 10.0) {
        Ok(v) => {
            let login = v.get("login").and_then(|b| b.as_bool()).unwrap_or(false);
            let field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            (login, format!("path {:?}, title {:?}", field("path"), field("title")))
        }
        Err(e) => (false, format!("page unreadable: {e}")),
    }
}

/// Poll until a composer is on the page (see `js_composer_helper!`).
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
                    return Err(rate_limited(RATE_LIMIT_MSG));
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
        assert!(JS_FIND_PICKER.contains("model-switcher-dropdown-button"));
        assert!(JS_PICKER_MENU.contains("composer-intelligence-picker-content"));
        assert!(keeps_current_model("current") && keeps_current_model(" Default "));
        assert!(!keeps_current_model("instant") && !keeps_current_model("pro"));
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
        assert!(JS_USER_TURNS.contains(r#"[data-message-author-role="${role}"]"#));
    }

    #[test]
    fn turn_probes_read_both_renderers() {
        // The newer renderer groups both roles under [data-turn-key] and may carry
        // no author-role attribute at all; every probe must still see its turns.
        // Behaviour is checked against DOM fixtures by scripts/check-dom.sh;
        // this only pins that every probe carries the shared helpers.
        for js in [JS_USER_TURNS, JS_STATE, JS_ASSISTANT_COUNT, JS_LAST_ASSISTANT] {
            assert!(js.contains("[data-user-message-bubble]"), "{js}");
            assert!(js.contains(r#"[data-conversation-role="assistant"]"#), "{js}");
            assert!(js.contains(r#"[data-content-search-unit-key$=":${role}"]"#), "{js}");
        }
        assert!(JS_USER_TURNS.contains("data-turn-id-container"));
        assert!(JS_CLICK_STOP.contains("composer-stop-button"));
        assert!(JS_STATE.contains("composer-stop-button"));
    }

    /// Writes the page probes for scripts/check-dom.sh. Ignored: it is a
    /// build step for that script, not a test.
    #[test]
    #[ignore]
    fn dump_probe_js() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/probe-js");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, js) in [
            ("state", JS_STATE),
            ("user_turns", JS_USER_TURNS),
            ("assistant_count", JS_ASSISTANT_COUNT),
            ("last_assistant", JS_LAST_ASSISTANT),
            ("composer", JS_COMPOSER),
            ("dismiss_dialog", JS_DISMISS_DIALOG),
            ("click_stop", JS_CLICK_STOP),
            ("clear_composer", JS_CLEAR_COMPOSER),
            ("composer_fingerprint", JS_COMPOSER_FINGERPRINT),
            ("insert_hello", &js_insert_text("hello")),
            ("find_picker", JS_FIND_PICKER),
            ("picker_menu", JS_PICKER_MENU),
        ] {
            std::fs::write(dir.join(format!("{name}.js")), js).unwrap();
        }
    }

    #[test]
    fn rate_limit_probes_match_localized_dialogs() {
        for js in [JS_COMPOSER, JS_STATE, JS_DISMISS_DIALOG] {
            for phrase in ["too many requests", "太多请求", "リクエストが多すぎます", "요청이 너무 많습니다"] {
                assert!(js.contains(phrase), "{phrase} missing from {js}");
            }
        }
        // A bare unanchored "ok" would click any button whose label contains it.
        assert!(JS_DISMISS_DIALOG.contains("/^(got it|ok|"));
    }

    #[test]
    fn a_task_cancel_flag_is_scoped_to_its_thread_and_call() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!cancel_requested());
        with_cancel_flag(flag.clone(), || {
            assert!(!cancel_requested());
            flag.store(true, Ordering::SeqCst);
            assert!(cancel_requested());
            // Another thread, another task: not cancelled.
            assert!(!std::thread::spawn(cancel_requested).join().unwrap());
        });
        // The next task on this thread starts clean.
        assert!(!cancel_requested());
    }

    fn turns(count: u64, ids: &[&str], known: &[&str]) -> UserTurns {
        UserTurns {
            count,
            ids: ids.iter().map(|s| s.to_string()).collect(),
            known: known.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn user_turn_rose_on_a_fresh_chat() {
        assert!(user_turn_rose(&turns(0, &[], &[]), &turns(1, &["u1"], &["u1"])));
        assert!(!user_turn_rose(&turns(0, &[], &[]), &turns(0, &[], &[])));
    }

    #[test]
    fn user_turn_rose_by_identity_despite_virtualization() {
        // Two old turns at baseline; one scrolls out as ours arrives: count flat.
        let base = turns(2, &["u1", "u2"], &["u1", "u2"]);
        assert!(user_turn_rose(&base, &turns(2, &["u2", "u3"], &["u1", "u2", "u3"])));
    }

    #[test]
    fn remounting_an_old_turn_is_not_a_submit() {
        // u1 was virtualized at baseline (container only); it remounts later.
        let base = turns(1, &["u2"], &["u1", "u2"]);
        assert!(!user_turn_rose(&base, &turns(2, &["u1", "u2"], &["u1", "u2"])));
    }

    #[test]
    fn grouped_renderer_identities_work_too() {
        let base = turns(1, &["group:a"], &["group:a"]);
        assert!(user_turn_rose(&base, &turns(2, &["group:a", "group:b"], &["group:a", "group:b"])));
    }

    #[test]
    fn user_turn_rose_falls_back_to_count_without_identities() {
        assert!(user_turn_rose(&turns(2, &[], &[]), &turns(3, &[], &[])));
        assert!(!user_turn_rose(&turns(2, &[], &[]), &turns(2, &[], &[])));
        // One rendered turn has no identity: the id test could miss it, so count.
        assert!(user_turn_rose(&turns(1, &["u1"], &["u1"]), &turns(2, &["u1"], &["u1"])));
    }

    #[test]
    fn user_turns_parse_from_probe_json() {
        let v = serde_json::json!({"count": 2, "ids": ["a", "b"], "known": ["a", "b", "c"]});
        assert_eq!(UserTurns::from_json(&v), Some(turns(2, &["a", "b"], &["a", "b", "c"])));
        assert_eq!(UserTurns::from_json(&serde_json::json!(3)), None);
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
    fn which_bin_finds_platform_shell() {
        let shell = if cfg!(windows) { "powershell.exe" } else { "sh" };
        assert!(which_bin(shell).is_some(), "{shell} should be findable on PATH");
    }

    fn typed(kind: ErrorKind) -> anyhow::Error {
        ChannelError::new(kind, "boom").into()
    }

    #[test]
    fn channel_error_survives_context_layers() {
        let e = typed(ErrorKind::RateLimited).context("resubmitting").context("outer");
        assert_eq!(channel_error(&e).map(|c| c.kind), Some(ErrorKind::RateLimited));
    }

    #[test]
    fn untyped_failures_are_typed_by_how_far_the_turn_got() {
        let before = classify(anyhow!("tab vanished"), false);
        let ce = channel_error(&before).unwrap();
        assert_eq!((ce.kind, ce.submitted), (ErrorKind::NotSubmitted, Submitted::No));
        assert!(format!("{before:#}").contains("tab vanished"), "the cause is kept");

        let after = classify(anyhow!("page swapped"), true);
        let ce = channel_error(&after).unwrap();
        assert_eq!((ce.kind, ce.submitted), (ErrorKind::Incomplete, Submitted::Yes));
    }

    #[test]
    fn a_typed_failure_keeps_its_kind_and_takes_the_turn_phase() {
        let e = classify(typed(ErrorKind::RateLimited).context("while polling"), true);
        let ce = channel_error(&e).unwrap();
        assert_eq!((ce.kind, ce.submitted), (ErrorKind::RateLimited, Submitted::Yes));
    }

    #[test]
    fn nothing_downgrades_an_ambiguous_submit() {
        for phase in [false, true] {
            let e = SubmitFailure::Ambiguous(anyhow!("no receipt")).into_error();
            let e = classify(e.context("x"), phase);
            let ce = channel_error(&e).map(|c| (c.kind, c.submitted));
            assert_eq!(ce, Some((ErrorKind::SubmitUnknown, Submitted::Unknown)));
        }
        // Also when the ambiguity was itself caused by a typed failure.
        let e = SubmitFailure::Ambiguous(typed(ErrorKind::RateLimited)).into_error();
        let e = classify(e, false);
        let ce = channel_error(&e).unwrap();
        assert_eq!((ce.kind, ce.submitted), (ErrorKind::RateLimited, Submitted::Unknown));
    }

    #[test]
    fn no_failure_maps_to_completed() {
        use ErrorKind::*;
        for k in [
            LoginRequired, RateLimited, SessionUnavailable, PageBlocked, NotSubmitted, SubmitUnknown,
            Incomplete, Busy, Duplicate, Cancelled, CancelRequested,
        ] {
            assert_ne!(k.status(), "completed", "{k:?}");
        }
        assert_eq!(Incomplete.status(), "incomplete");
        assert_eq!(RateLimited.status(), "unavailable");
    }

    fn turn(done: bool, text: &str, finish: Option<&str>, reason: Option<&str>) -> ServerTurn {
        ServerTurn {
            done,
            text: text.into(),
            finish: finish.map(Into::into),
            reason: reason.map(Into::into),
        }
    }

    #[test]
    fn a_stopped_turn_is_incomplete_not_a_reply() {
        // Exactly what the record showed after the stop button, live.
        let e = judge_record(turn(true, "partial essay", Some("interrupted"), Some("client_stopped")))
            .unwrap_err();
        let ce = channel_error(&e).unwrap();
        assert_eq!((ce.kind, ce.submitted), (ErrorKind::Incomplete, Submitted::Yes));
        assert!(e.to_string().contains("client_stopped"), "{e}");

        let e = judge_record(turn(true, "", Some("max_tokens"), None)).unwrap_err();
        assert!(e.to_string().contains("length limit"), "{e}");
    }

    #[test]
    fn a_real_finish_is_a_reply_and_an_open_turn_is_not_yet() {
        assert_eq!(judge_record(turn(true, "done", Some("stop"), None)).unwrap(), Some("done".into()));
        // An unknown finish type is not treated as a failure.
        assert_eq!(judge_record(turn(true, "done", Some("new_kind"), None)).unwrap(), Some("done".into()));
        assert_eq!(judge_record(turn(true, "done", None, None)).unwrap(), Some("done".into()));
        assert_eq!(judge_record(turn(false, "streaming", None, None)).unwrap(), None);
        assert_eq!(judge_record(turn(true, "  ", Some("stop"), None)).unwrap(), None);
    }

    #[test]
    fn the_record_walk_stops_at_this_turns_prompt() {
        let js = js_server_final("c-1");
        assert!(js.contains("m.author.role === 'user') break"), "must not reach the previous turn");
        assert!(js.contains("finish_details"));
    }

    #[test]
    fn opening_a_project_in_place_only_clicks_the_project_page_link() {
        let js = js_open_project_in_place("g-p-abc");
        assert!(js.contains(r"/\/project\/?$/"), "{js}");
    }
}
