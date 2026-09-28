//! The daily "new version" notice, end to end through the real binary: one
//! line on stderr, nothing on stdout, and the skip rules. A fresh cache holds
//! the "latest" version, so nothing here touches the network; `status` is used
//! because it never touches the browser either.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const OPT_OUTS: [&str; 3] = ["CI", "CHATGPT_USE_NO_UPDATE_CHECK", "USE_NO_UPDATE_CHECK"];

fn temp_home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chatgpt-use-notice-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let cache = d.join("cache").join("chatgpt-use");
    std::fs::create_dir_all(&cache).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        cache.join("update-check.json"),
        format!(r#"{{"checked_at": {now}, "latest": "999.0.0"}}"#),
    )
    .unwrap();
    d
}

/// Run the binary in a temp home with every opt-out cleared (GitHub Actions
/// sets CI=true), plus `extra`.
fn run(home: &PathBuf, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chatgpt-use"));
    cmd.args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env_remove("GITHUB_TOKEN");
    for k in OPT_OUTS {
        cmd.env_remove(k);
    }
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn expected_line() -> String {
    format!(
        "chatgpt-use 999.0.0 is available (you have {}). Upgrade: chatgpt-use upgrade",
        env!("CARGO_PKG_VERSION")
    )
}

#[test]
fn notice_is_one_line_on_stderr_and_never_on_stdout() {
    let home = temp_home("shown");
    let out = run(&home, &["status", "no-such-request"], &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.stdout.is_empty(),
        "stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        stderr
            .lines()
            .filter(|l| l.contains("is available"))
            .count(),
        1,
        "{stderr}"
    );
    assert_eq!(stderr.lines().next().unwrap(), expected_line());
}

#[test]
fn opt_outs_suppress_the_notice() {
    let home = temp_home("optout");
    for var in OPT_OUTS {
        let out = run(&home, &["status", "no-such-request"], &[(var, "1")]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("is available"), "{var}: {stderr}");
    }
    let out = run(&home, &["status", "no-such-request"], &[("CI", "true")]);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("is available"));
}

#[test]
fn skipped_for_version_help_and_upgrade_help() {
    let home = temp_home("skip");
    for args in [
        &["--version"][..],
        &["--help"],
        &["-h"],
        &["-V"],
        &["upgrade", "--help"],
        &["status", "--help"],
    ] {
        let out = run(&home, args, &[]);
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!all.contains("is available"), "{args:?}: {all}");
    }
}
