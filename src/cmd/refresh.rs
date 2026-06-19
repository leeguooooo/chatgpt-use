//! `chatgpt-use refresh` — re-sync the `chatgpt-use` connector in ChatGPT.
//!
//! After the `mcp` server restarts (a redeploy, a launchd KeepAlive bounce),
//! ChatGPT keeps a cached `tools/list` and may mark the connector stale until a
//! manual "Refresh". This drives that click for you.
//!
//! The flow was reverse-engineered against the live ChatGPT settings UI (2026-06):
//!   1. Deep-link `https://chatgpt.com/#settings/Connectors` opens Settings on
//!      the Apps pane (a `[role="dialog"]` modal) listing the connectors.
//!   2. Each connector is a `<button>` whose text is its name; ours renders as
//!      `chatgpt-useDEV` ("DEV" is a badge). Clicking it opens the connector
//!      detail (the URL gains `?connector=asdk_app_…`).
//!   3. The detail pane has a `<button>` with the exact text `Refresh`. Clicking
//!      it re-runs `tools/list` against our server.
//! Both buttons respond to a JS `.click()` (verified), so we do the whole thing
//! in one page-side script rather than chasing CDP coordinates.
//!
//! Best-effort: on a miss it prints the controls it actually saw so you can
//! finish by hand. Pass `--url` if the deep-link ever changes.

use crate::channel::{Channel, ChannelOptions};
use crate::cli::RefreshArgs;
use anyhow::{bail, Result};

// Deep-link that opens Settings → Apps (Connectors). Override with --url.
const DEFAULT_SETTINGS_URL: &str = "https://chatgpt.com/#settings/Connectors";

pub fn run(args: &RefreshArgs) -> Result<()> {
    // No chat, no project, no model picker — just a logged-in page to drive.
    let opts = ChannelOptions {
        profile: args.channel.profile.clone(),
        session: args.channel.session.clone(),
        project: String::new(),
        timeout_secs: args.channel.timeout.max(60),
        model: None,
    };

    let channel = Channel::connect(&opts)?;
    let result = do_refresh(&channel, args);
    channel.close();
    result
}

fn do_refresh(channel: &Channel, args: &RefreshArgs) -> Result<()> {
    let url = args.url.clone().unwrap_or_else(|| DEFAULT_SETTINGS_URL.to_string());
    eprintln!("opening connector settings: {url}");
    channel.open(&url)?;

    let res = channel.eval(&js_open_and_refresh(&args.connector))?;
    let ok = res.get("ok").and_then(|b| b.as_bool()).unwrap_or(false);

    if ok {
        let conn = res.get("connector").and_then(|s| s.as_str()).unwrap_or("");
        let rb = res.get("refresh").and_then(|s| s.as_str()).unwrap_or("Refresh");
        eprintln!("opened {conn:?}, clicked {rb:?}");
        eprintln!("refresh: done — ChatGPT re-ran tools/list against the server.");
        crate::ledger::record("refresh", serde_json::json!({ "connector": args.connector, "ok": true }));
        return Ok(());
    }

    let step = res.get("step").and_then(|s| s.as_str()).unwrap_or("unknown");
    let seen: Vec<String> = res
        .get("controls")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    crate::ledger::record(
        "refresh",
        serde_json::json!({ "connector": args.connector, "ok": false, "step": step }),
    );
    bail!(
        "refresh failed at step '{step}' for connector {:?}.\n\
         Finish manually: ChatGPT → Settings → Apps → {} → Refresh.\n\
         Controls I saw: {}",
        args.connector,
        args.connector,
        if seen.is_empty() { "(none)".into() } else { seen.join(" | ") }
    );
}

/// One self-contained page script: open the connector's detail, then click its
/// Refresh button. Returns {ok, connector, refresh} or {ok:false, step, controls}.
fn js_open_and_refresh(name: &str) -> String {
    let name_json = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(async () => {{
  const name = {name_json}.toLowerCase();
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  const dialog = () => document.querySelector('[role="dialog"]');

  // Wait for the settings modal to render after navigation.
  let d = null;
  for (let i = 0; i < 20; i++) {{ d = dialog(); if (d && /connector|apps/i.test(d.textContent||'')) break; await sleep(300); }}
  if (!d) return JSON.stringify({{ok: false, step: 'no-settings-dialog', controls: []}});

  const buttonsIn = root => [...root.querySelectorAll('button')]
    .filter(b => b.getAttribute('role') !== 'tab');
  const labels = root => buttonsIn(root).map(b => (b.textContent||'').trim()).filter(t => t && t.length <= 40);

  // Step 1: open the connector whose button text STARTS WITH the name
  // (tolerates a trailing "DEV"/badge). Skip the nav tabs and unrelated rows.
  let connBtn = null;
  for (const b of buttonsIn(d)) {{
    const t = (b.textContent || '').trim().toLowerCase();
    if (t.startsWith(name) && t.length <= name.length + 12) {{ connBtn = b; break; }}
  }}
  if (!connBtn) return JSON.stringify({{ok: false, step: 'connector-not-found', controls: labels(d).slice(0,25)}});
  const connLabel = (connBtn.textContent||'').trim();
  connBtn.click();

  // Step 2: wait for the detail pane, then click the Refresh button.
  let refreshBtn = null;
  for (let i = 0; i < 20; i++) {{
    await sleep(300);
    const dd = dialog(); if (!dd) continue;
    for (const b of buttonsIn(dd)) {{
      const t = (b.textContent||'').trim();
      if (/^(refresh|re-?sync|reconnect|reload)$/i.test(t)) {{ refreshBtn = b; break; }}
    }}
    if (refreshBtn) break;
  }}
  if (!refreshBtn) return JSON.stringify({{ok: false, step: 'refresh-not-found', controls: labels(dialog()||d).slice(0,25)}});
  const rLabel = (refreshBtn.textContent||'').trim();
  refreshBtn.click();
  await sleep(1500);
  return JSON.stringify({{ok: true, connector: connLabel, refresh: rLabel}});
}})()"#
    )
}
