//! Account-wide throttle discipline, shared by every process on this machine.
//!
//! chatgpt.com throttles the ACCOUNT by request count, and the dialog going
//! away does not mean the cooldown has. The page-side wait in `Channel::send`
//! only covers the turn it happens in. The next `ask` a minute later knew
//! nothing about it, opened the page again (about 45 requests), and pushed the
//! account further into the throttle.
//!
//! Two pieces, after miuuyy/codex-chatgpt-web's "stop on the first limit,
//! let the cooldown clear" policy and its local send receipts:
//!
//! - **Cooldown.** A rate-limit hit writes `~/.chatgpt-use/throttle.json` with
//!   an `until` time. Until then every new channel refuses before touching the
//!   browser. Repeat hits within an hour lengthen it. Setting
//!   `CHATGPT_USE_IGNORE_COOLDOWN=1` skips the check.
//! - **Pace warning.** Each page load and each submitted prompt is a ledger
//!   event. Before a new one, the last hour is counted and a warning printed
//!   once the count nears what has tripped the throttle before. It only warns;
//!   ChatGPT publishes no quota to enforce.
//!
//! The decisions are pure functions over plain values so they test offline.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Cooldown after the first hit, then after repeats within [`REPEAT_WINDOW`].
const COOLDOWNS: &[u64] = &[5 * 60, 15 * 60, 30 * 60];
/// Hits further apart than this start the escalation over.
const REPEAT_WINDOW: u64 = 60 * 60;

/// Page loads per hour before warning. About 23 full loads in one afternoon
/// tripped the throttle on the account this was built against (see AGENTS.md).
const DEFAULT_WARN_LOADS: u64 = 15;
/// Submitted prompts per hour before warning.
const DEFAULT_WARN_SENDS: u64 = 40;
const PACE_WINDOW: u64 = 60 * 60;

/// Ledger event kinds this module counts.
pub const EVENT_PAGE_LOAD: &str = "page_load";
pub const EVENT_SEND: &str = "send";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cooldown {
    /// Unix seconds when new channels may open again.
    pub until: u64,
    /// When the latest rate-limit hit was recorded.
    pub last_hit: u64,
    /// Consecutive hits, each within [`REPEAT_WINDOW`] of the one before.
    pub hits: u32,
}

/// The cooldown after a hit at `now`, given the one on record.
pub fn after_hit(prev: Option<Cooldown>, now: u64) -> Cooldown {
    let hits = match prev {
        Some(p) if now.saturating_sub(p.last_hit) < REPEAT_WINDOW => p.hits.saturating_add(1),
        _ => 1,
    };
    let step = COOLDOWNS[(hits as usize - 1).min(COOLDOWNS.len() - 1)];
    // Never shorten a cooldown already running.
    let until = (now + step).max(prev.map_or(0, |p| p.until));
    Cooldown { until, last_hit: now, hits }
}

/// Seconds of cooldown left at `now`, if any.
pub fn remaining(state: Option<Cooldown>, now: u64) -> Option<u64> {
    state.map(|s| s.until.saturating_sub(now)).filter(|&left| left > 0)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cooldown_path(dir: &Path) -> PathBuf {
    dir.join("throttle.json")
}

fn load_from(dir: &Path) -> Option<Cooldown> {
    let body = std::fs::read_to_string(cooldown_path(dir)).ok()?;
    serde_json::from_str(&body).ok()
}

fn note_hit_in(dir: &Path, now: u64) -> std::io::Result<Cooldown> {
    let next = after_hit(load_from(dir), now);
    std::fs::create_dir_all(dir)?;
    // Write-then-rename, so a reader never sees half a file.
    let tmp = dir.join(format!("throttle.json.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(&next)?)?;
    std::fs::rename(&tmp, cooldown_path(dir))?;
    Ok(next)
}

/// Record a rate-limit hit. Best effort: a write error only warns.
pub fn note_rate_limited() {
    match note_hit_in(&crate::ledger::ledger_dir(), now_secs()) {
        Ok(c) => eprintln!(
            "rate-limited: pausing new ChatGPT runs for {} (hit {} within the hour)",
            human(c.until.saturating_sub(c.last_hit)),
            c.hits
        ),
        Err(e) => eprintln!("warning: could not record the rate-limit cooldown: {e}"),
    }
}

/// Seconds of account cooldown left, or `None` when clear or overridden.
pub fn cooldown_left() -> Option<u64> {
    if env_flag("CHATGPT_USE_IGNORE_COOLDOWN") {
        return None;
    }
    remaining(load_from(&crate::ledger::ledger_dir()), now_secs())
}

/// The message for a run refused during cooldown.
pub fn refusal(left: u64) -> String {
    format!(
        "chatgpt.com rate-limited this account recently; new runs are paused for {} more so \
         the cooldown can clear (every retry costs more requests and extends it). \
         Set CHATGPT_USE_IGNORE_COOLDOWN=1 to go anyway.",
        human(left)
    )
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0")
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn human(secs: u64) -> String {
    if secs >= 60 {
        format!("{} min", secs.div_ceil(60))
    } else {
        format!("{secs}s")
    }
}

/// Count `kind` events in ledger lines no older than `window` before `now`.
pub fn count_recent<'a>(
    lines: impl Iterator<Item = &'a str>,
    kind: &str,
    now: u64,
    window: u64,
) -> u64 {
    let since = now.saturating_sub(window);
    lines
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["kind"] == kind && v["ts"].as_u64().is_some_and(|ts| ts >= since))
        .count() as u64
}

/// The pace warning for `count` recent `kind` events, if it is due.
pub fn pace_warning(kind: &str, count: u64, limit: u64) -> Option<String> {
    if limit == 0 || count < limit {
        return None;
    }
    let what = if kind == EVENT_PAGE_LOAD { "ChatGPT page loads" } else { "prompts sent" };
    Some(format!(
        "warning: {count} {what} in the last hour on this machine — near the pace that has \
         tripped chatgpt.com's 'Too many requests' throttle before. Consider pausing."
    ))
}

/// The tail of the ledger: enough for an hour of events without reading a
/// file that grows forever.
fn ledger_tail(dir: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 512 * 1024;
    let Ok(mut f) = std::fs::File::open(dir.join("ledger.jsonl")) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > TAIL && f.seek(SeekFrom::Start(len - TAIL)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    // A mid-file start lands inside a line, which simply fails to parse.
    String::from_utf8_lossy(&buf).into_owned()
}

/// Record one `kind` event, warning first if the last hour is already busy.
pub fn note_event(kind: &str, data: serde_json::Value) {
    let limit = if kind == EVENT_PAGE_LOAD {
        env_u64("CHATGPT_USE_WARN_LOADS_PER_HOUR", DEFAULT_WARN_LOADS)
    } else {
        env_u64("CHATGPT_USE_WARN_SENDS_PER_HOUR", DEFAULT_WARN_SENDS)
    };
    let tail = ledger_tail(&crate::ledger::ledger_dir());
    let count = count_recent(tail.lines(), kind, now_secs(), PACE_WINDOW);
    if let Some(w) = pace_warning(kind, count + 1, limit) {
        eprintln!("{w}");
    }
    crate::ledger::record(kind, data);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_hit_cools_down_five_minutes() {
        assert_eq!(after_hit(None, 1000), Cooldown { until: 1300, last_hit: 1000, hits: 1 });
    }

    #[test]
    fn repeat_hits_within_the_hour_escalate_and_cap() {
        let a = after_hit(None, 0);
        let b = after_hit(Some(a), 600);
        assert_eq!((b.hits, b.until), (2, 600 + 15 * 60));
        let c = after_hit(Some(b), 1200);
        assert_eq!((c.hits, c.until), (3, 1200 + 30 * 60));
        let d = after_hit(Some(c), 1800);
        assert_eq!((d.hits, d.until), (4, 1800 + 30 * 60));
    }

    #[test]
    fn a_hit_after_a_quiet_hour_starts_over() {
        let a = Cooldown { until: 1800, last_hit: 0, hits: 3 };
        assert_eq!(after_hit(Some(a), REPEAT_WINDOW).hits, 1);
    }

    #[test]
    fn a_hit_never_shortens_a_running_cooldown() {
        let long = Cooldown { until: 10_000, last_hit: 9_000, hits: 3 };
        // 2h later the escalation restarts at 5 min, but `until` was later still.
        let quiet = Cooldown { until: 20_000, last_hit: 0, hits: 1 };
        assert_eq!(after_hit(Some(quiet), REPEAT_WINDOW + 1).until, 20_000);
        assert!(after_hit(Some(long), 9_100).until >= 10_000);
    }

    #[test]
    fn remaining_is_none_once_expired() {
        let c = Cooldown { until: 500, last_hit: 200, hits: 1 };
        assert_eq!(remaining(Some(c), 400), Some(100));
        assert_eq!(remaining(Some(c), 500), None);
        assert_eq!(remaining(None, 0), None);
    }

    #[test]
    fn cooldown_persists_across_processes_via_the_file() {
        let dir = std::env::temp_dir().join(format!("cgu-throttle-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_from(&dir), None);
        note_hit_in(&dir, 100).unwrap();
        let second = note_hit_in(&dir, 200).unwrap();
        assert_eq!(load_from(&dir), Some(second));
        assert_eq!(second.hits, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn counts_only_recent_events_of_the_kind() {
        let lines = [
            r#"{"v":1,"ts":100,"kind":"page_load","data":{}}"#,
            r#"{"v":1,"ts":4000,"kind":"page_load","data":{}}"#,
            r#"{"v":1,"ts":4100,"kind":"send","data":{}}"#,
            r#"{"v":1,"ts":4200,"kind":"page_load","data":{}}"#,
            "ad\":1,\"kind\":\"page_load\"}", // a torn first line from a mid-file seek
        ];
        assert_eq!(count_recent(lines.iter().copied(), EVENT_PAGE_LOAD, 4300, 3600), 2);
        assert_eq!(count_recent(lines.iter().copied(), EVENT_SEND, 4300, 3600), 1);
    }

    #[test]
    fn pace_warning_fires_at_the_limit() {
        assert_eq!(pace_warning(EVENT_PAGE_LOAD, 14, 15), None);
        assert!(pace_warning(EVENT_PAGE_LOAD, 15, 15).unwrap().contains("page loads"));
        assert!(pace_warning(EVENT_SEND, 40, 40).unwrap().contains("prompts sent"));
        assert_eq!(pace_warning(EVENT_SEND, 99, 0), None, "0 disables the warning");
    }

    #[test]
    fn ledger_tail_reads_a_missing_file_as_empty() {
        assert_eq!(ledger_tail(Path::new("/nonexistent/cgu")), "");
    }
}
