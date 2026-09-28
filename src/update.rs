//! Update check shared by `upgrade` and the once-a-day notice — the *-use
//! family upgrade convention
//! (https://github.com/leeguooooo/plugins/blob/main/docs/upgrade.md).
//!
//! The latest version is the newest non-prerelease GitHub release. Any command
//! checks it at most once a day (cached, 2 s timeout) and prints one line to
//! stderr when it is newer; nothing is installed until `chatgpt-use upgrade`.
//! Skipped under `CI`, `CHATGPT_USE_NO_UPDATE_CHECK` or the family-wide
//! `USE_NO_UPDATE_CHECK`.
//!
//! The network goes through `curl` (what install.sh already needs), so the
//! binary carries no HTTP client or TLS stack.

use crate::platform;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const NAME: &str = "chatgpt-use";
pub const REPO: &str = "leeguooooo/chatgpt-use";
pub const CHECK_INTERVAL_SECS: u64 = 86_400;
/// The daily check must not slow a command down.
pub const NOTICE_TIMEOUT_SECS: u64 = 2;
/// An explicit `upgrade` can wait a little longer.
pub const FETCH_TIMEOUT_SECS: u64 = 10;
const NO_CHECK_VARS: [&str; 3] = ["CI", "CHATGPT_USE_NO_UPDATE_CHECK", "USE_NO_UPDATE_CHECK"];

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn releases_url() -> String {
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

/// `v1.2.3` / `1.2.3` → `[1, 2, 3]`; anything else (a prerelease suffix,
/// words) → None.
pub fn parse_version(s: &str) -> Option<Vec<u64>> {
    let s = s.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    let parts: Option<Vec<u64>> = s.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| (2..=4).contains(&p.len()))
}

/// Whether `latest` is a strictly newer version than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(mut l), Some(mut c)) => {
            let n = l.len().max(c.len());
            l.resize(n, 0);
            c.resize(n, 0);
            l > c
        }
        _ => false,
    }
}

/// `X.Y.Z` from a `releases/latest` body; None for drafts, prereleases and
/// tags that are not a plain version.
pub fn parse_release(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let tag = v["tag_name"].as_str()?;
    parse_version(tag)?;
    Some(tag.trim().trim_start_matches('v').to_string())
}

/// GET `url` with curl. `GITHUB_TOKEN`, when set, goes in through a curl
/// config on stdin so it never shows up in the process list.
pub fn http_get(url: &str, timeout_secs: u64, accept: &str) -> Result<String, String> {
    let mut cfg = format!(
        "header = \"Accept: {accept}\"\nheader = \"User-Agent: {NAME}/{}\"\n",
        current_version()
    );
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let token = token.trim();
        if !token.is_empty()
            && url.starts_with("https://api.github.com/")
            && !token.contains(['"', '\n', '\\'])
        {
            cfg.push_str(&format!("header = \"Authorization: Bearer {token}\"\n"));
        }
    }
    let mut child = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            &timeout_secs.to_string(),
            "-K",
            "-",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(cfg.as_bytes());
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("curl failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("curl exited with {}", out.status)
        } else {
            err
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The newest non-prerelease release version from GitHub.
pub fn fetch_latest(timeout_secs: u64) -> Result<String, String> {
    let body = http_get(&releases_url(), timeout_secs, "application/vnd.github+json")?;
    parse_release(&body).ok_or_else(|| "no usable release in the GitHub response".to_string())
}

/// Any opt-out variable set to a non-empty value turns the daily check off.
pub fn disabled(env: &dyn Fn(&str) -> Option<String>) -> bool {
    NO_CHECK_VARS
        .iter()
        .any(|k| env(k).is_some_and(|v| !v.is_empty()))
}

/// `${XDG_CACHE_HOME:-~/.cache}/chatgpt-use/update-check.json`.
pub fn cache_path(env: &dyn Fn(&str) -> Option<String>, home: Option<PathBuf>) -> Option<PathBuf> {
    let base = match env("XDG_CACHE_HOME").filter(|v| !v.trim().is_empty()) {
        Some(x) => PathBuf::from(x),
        None => home?.join(".cache"),
    };
    Some(base.join(NAME).join("update-check.json"))
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CheckCache {
    pub checked_at: u64,
    pub latest: Option<String>,
}

pub fn read_cache(path: &Path) -> Option<CheckCache> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub fn write_cache(path: &Path, cache: &CheckCache) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // A per-process temp name: two commands refreshing at once must not
    // interleave writes into one temp file and rename a half-written cache.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if let Ok(body) = serde_json::to_string(cache) {
        // create_new: never follow or reuse a file already sitting there.
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .and_then(|mut f| f.write_all(body.as_bytes()));
        match written {
            Ok(()) => {
                if std::fs::rename(&tmp, path).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            // AlreadyExists is someone else's file: leave it alone.
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&tmp);
            }
            Err(_) => {}
        }
    }
}

/// Latest version, from the cache while it is under a day old, else from
/// `fetch`. A failed fetch still stamps `checked_at` (keeping the last known
/// version) so an offline machine is not retried on every call.
pub fn cached_latest(
    path: &Path,
    now: u64,
    fetch: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let prev = read_cache(path);
    let prev_latest = prev
        .as_ref()
        .and_then(|c| c.latest.clone())
        .filter(|v| parse_version(v).is_some());
    if let Some(c) = &prev {
        if now >= c.checked_at && now - c.checked_at < CHECK_INTERVAL_SECS {
            return prev_latest;
        }
    }
    let latest = fetch().or(prev_latest);
    write_cache(
        path,
        &CheckCache {
            checked_at: now,
            latest: latest.clone(),
        },
    );
    latest
}

pub fn notice_line(latest: &str, current: &str) -> String {
    format!("{NAME} {latest} is available (you have {current}). Upgrade: {NAME} upgrade")
}

/// The notice to print, if any. Pure apart from the cache file and `fetch`.
pub fn notice(
    env: &dyn Fn(&str) -> Option<String>,
    home: Option<PathBuf>,
    now: u64,
    current: &str,
    fetch: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if disabled(env) {
        return None;
    }
    let path = cache_path(env, home)?;
    let latest = cached_latest(&path, now, fetch)?;
    is_newer(&latest, current).then(|| notice_line(&latest, current))
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn real_env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

/// Once a day, write the "new version" line to `out` (stderr in `main`).
/// Never fails and never touches stdout.
pub fn maybe_notify(out: &mut dyn Write) {
    let line = notice(
        &real_env,
        platform::home_dir(),
        now(),
        current_version(),
        || fetch_latest(NOTICE_TIMEOUT_SECS).ok(),
    );
    if let Some(line) = line {
        let _ = writeln!(out, "{line}");
    }
}

/// Record a version `upgrade` just read, so the notice agrees with it.
pub fn remember_latest(latest: &str) {
    if let Some(path) = cache_path(&real_env, platform::home_dir()) {
        write_cache(
            &path,
            &CheckCache {
                checked_at: now(),
                latest: Some(latest.to_string()),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "chatgpt-use-update-test-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.0.5", "0.0.4"));
        assert!(is_newer("v0.1.0", "0.0.9"));
        assert!(is_newer("0.0.10", "0.0.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.0.4", "0.0.4"));
        assert!(!is_newer("0.0.3", "0.0.4"));
        assert!(!is_newer("1.0.0-rc1", "0.0.4"));
        assert!(!is_newer("garbage", "0.0.4"));
        assert_eq!(parse_version("v1.2.3"), Some(vec![1, 2, 3]));
        assert_eq!(parse_version("7"), None);
    }

    #[test]
    fn release_parsing_skips_prereleases() {
        assert_eq!(
            parse_release(r#"{"tag_name":"v0.0.5","prerelease":false,"draft":false}"#),
            Some("0.0.5".into())
        );
        assert_eq!(
            parse_release(r#"{"tag_name":"v0.0.5","prerelease":true}"#),
            None
        );
        assert_eq!(parse_release(r#"{"tag_name":"nightly"}"#), None);
        assert_eq!(
            parse_release(r#"{"message":"API rate limit exceeded"}"#),
            None
        );
        assert_eq!(parse_release("<html>"), None);
    }

    #[test]
    fn opt_out_env_vars() {
        assert!(!disabled(&env_of(&[])));
        for var in NO_CHECK_VARS {
            assert!(disabled(&env_of(&[(var, "1")])), "{var}");
            assert!(!disabled(&env_of(&[(var, "")])), "{var} empty");
        }
        // Disabled means no fetch and no cache file at all.
        let home = temp_dir("optout");
        for var in NO_CHECK_VARS {
            let got = notice(
                &env_of(&[(var, "true")]),
                Some(home.clone()),
                1000,
                "0.0.4",
                || panic!("fetched"),
            );
            assert_eq!(got, None);
        }
        assert!(!home.join(".cache").exists());
    }

    #[test]
    fn cache_path_follows_xdg() {
        let home = PathBuf::from("/home/u");
        assert_eq!(
            cache_path(&env_of(&[]), Some(home.clone())),
            Some(PathBuf::from(
                "/home/u/.cache/chatgpt-use/update-check.json"
            ))
        );
        assert_eq!(
            cache_path(&env_of(&[("XDG_CACHE_HOME", "/x")]), Some(home)),
            Some(PathBuf::from("/x/chatgpt-use/update-check.json"))
        );
    }

    #[test]
    fn throttled_to_once_per_24h() {
        let home = temp_dir("throttle");
        let env = env_of(&[]);
        let fetches = std::cell::Cell::new(0);
        let fetch = || {
            fetches.set(fetches.get() + 1);
            Some("9.9.9".to_string())
        };
        let t0 = 1_000_000;
        assert!(notice(&env, Some(home.clone()), t0, "0.0.4", fetch).is_some());
        assert!(notice(
            &env,
            Some(home.clone()),
            t0 + CHECK_INTERVAL_SECS - 1,
            "0.0.4",
            fetch
        )
        .is_some());
        assert_eq!(
            fetches.get(),
            1,
            "second call within 24 h must use the cache"
        );
        notice(
            &env,
            Some(home.clone()),
            t0 + CHECK_INTERVAL_SECS,
            "0.0.4",
            fetch,
        );
        assert_eq!(fetches.get(), 2, "a day later it checks again");
        let cache = read_cache(&home.join(".cache/chatgpt-use/update-check.json")).unwrap();
        assert_eq!(
            cache,
            CheckCache {
                checked_at: t0 + CHECK_INTERVAL_SECS,
                latest: Some("9.9.9".into())
            }
        );
    }

    #[test]
    fn failed_check_is_silent_and_still_stamps_checked_at() {
        let home = temp_dir("offline");
        let env = env_of(&[]);
        let fetches = std::cell::Cell::new(0);
        let fail = || {
            fetches.set(fetches.get() + 1);
            None
        };
        assert_eq!(notice(&env, Some(home.clone()), 5000, "0.0.4", fail), None);
        assert_eq!(notice(&env, Some(home.clone()), 5001, "0.0.4", fail), None);
        assert_eq!(fetches.get(), 1);
        let cache = read_cache(&home.join(".cache/chatgpt-use/update-check.json")).unwrap();
        assert_eq!(
            cache,
            CheckCache {
                checked_at: 5000,
                latest: None
            }
        );
    }

    #[test]
    fn failed_check_keeps_last_known_version() {
        let home = temp_dir("keep");
        let path = home.join(".cache/chatgpt-use/update-check.json");
        write_cache(
            &path,
            &CheckCache {
                checked_at: 1,
                latest: Some("9.9.9".into()),
            },
        );
        let got = notice(
            &env_of(&[]),
            Some(home),
            1 + CHECK_INTERVAL_SECS,
            "0.0.4",
            || None,
        );
        assert_eq!(
            got.as_deref(),
            Some("chatgpt-use 9.9.9 is available (you have 0.0.4). Upgrade: chatgpt-use upgrade")
        );
    }

    #[test]
    fn silent_when_current_or_newer() {
        let home = temp_dir("current");
        assert_eq!(
            notice(&env_of(&[]), Some(home.clone()), 10, "0.0.4", || Some(
                "0.0.4".into()
            )),
            None
        );
        let home = temp_dir("ahead");
        assert_eq!(
            notice(&env_of(&[]), Some(home), 10, "0.0.5", || Some(
                "0.0.4".into()
            )),
            None
        );
    }

    #[test]
    fn clock_going_backwards_rechecks() {
        let home = temp_dir("clock");
        let path = home.join(".cache/chatgpt-use/update-check.json");
        write_cache(
            &path,
            &CheckCache {
                checked_at: 10_000,
                latest: Some("0.0.1".into()),
            },
        );
        let got = notice(&env_of(&[]), Some(home), 50, "0.0.4", || {
            Some("9.9.9".into())
        });
        assert!(got.is_some());
    }
}
