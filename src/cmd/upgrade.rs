//! `upgrade [--check] [--json]` — install the latest GitHub release of
//! chatgpt-use through the route it was installed with, then refresh every
//! installed copy of the skill (the *-use family upgrade convention).
//!
//! Exit codes: 0 success (upgraded, already current, or a check that ran),
//! 2 the check or download failed, 1 anything else.

use crate::cli::UpgradeArgs;
use crate::platform;
use crate::update::{self, NAME, REPO};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SkillInstall {
    /// claude-plugin | git | copy
    pub channel: String,
    pub path: String,
    /// The command that refreshes it.
    pub update: String,
}

#[derive(Debug, Serialize)]
struct Report<'a> {
    name: &'a str,
    current: &'a str,
    latest: Option<&'a str>,
    update_available: bool,
    skills: &'a [SkillInstall],
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

/// How this binary was installed, judged from where it lives.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// install.sh / install.ps1 put the release binary in this directory.
    Installer(PathBuf),
    /// `cargo install --git …` (lives in ~/.cargo/bin).
    Cargo,
    /// A local `cargo build` (…/target/{debug,release}); nothing to install.
    SourceBuild(PathBuf),
}

pub fn run(args: &UpgradeArgs) -> i32 {
    let current = update::current_version();
    let latest = update::fetch_latest(update::FETCH_TIMEOUT_SECS);
    if let Ok(v) = &latest {
        update::remember_latest(v);
    }
    let skills = platform::home_dir()
        .map(|h| find_skill_installs(&h))
        .unwrap_or_default();
    let available = latest.as_ref().is_ok_and(|l| update::is_newer(l, current));

    if args.json {
        let report = Report {
            name: NAME,
            current,
            latest: latest.as_ref().ok().map(String::as_str),
            update_available: available,
            skills: &skills,
            error: latest.as_ref().err().map(String::as_str),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        return if latest.is_ok() { 0 } else { 2 };
    }
    let latest = match latest {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "error: could not read the latest release from {}: {e}",
                update::releases_url()
            );
            return 2;
        }
    };
    println!("{}", status_line(current, &latest));
    if args.check {
        for s in &skills {
            println!("skill [{}] {} (refresh: {})", s.channel, s.path, s.update);
        }
        return 0;
    }

    let mut rc = 0;
    if available {
        let route = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .map(|p| route_for(&p))
            .unwrap_or(Route::Installer(default_install_dir()));
        rc = install(&route, &latest);
        if rc == 0 {
            println!("upgraded {NAME} {current} -> {latest}");
            println!("release notes: https://github.com/{REPO}/releases/tag/v{latest}");
        }
    }
    for s in &skills {
        refresh_skill(s);
    }
    rc
}

pub fn status_line(current: &str, latest: &str) -> String {
    if update::is_newer(latest, current) {
        format!("{NAME} {current} -> {latest}")
    } else {
        format!("{NAME} {current} is up to date")
    }
}

fn default_install_dir() -> PathBuf {
    platform::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("bin")
}

pub fn route_for(exe: &Path) -> Route {
    let comps: Vec<String> = exe
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let in_build_dir = dir
        .file_name()
        .is_some_and(|n| n == "debug" || n == "release");
    if comps.windows(2).any(|w| w[0] == ".cargo" && w[1] == "bin") {
        Route::Cargo
    } else if in_build_dir && comps.iter().any(|c| c == "target") {
        Route::SourceBuild(dir)
    } else {
        Route::Installer(dir)
    }
}

fn run_status(cmd: &mut Command) -> Result<bool, String> {
    cmd.status().map(|s| s.success()).map_err(|e| e.to_string())
}

/// Step 1: install `latest` through `route`. 0 ok, 2 download/install failed.
fn install(route: &Route, latest: &str) -> i32 {
    match route {
        Route::SourceBuild(dir) => {
            println!(
                "cli: {} is a source build; update it with `git pull && cargo build --release`",
                dir.display()
            );
            1
        }
        Route::Cargo => {
            let tag = format!("v{latest}");
            let url = format!("https://github.com/{REPO}");
            println!("cli: cargo install --git {url} --tag {tag} --locked --force");
            match run_status(Command::new("cargo").args([
                "install", "--git", &url, "--tag", &tag, "--locked", "--force",
            ])) {
                Ok(true) => 0,
                Ok(false) => 2,
                Err(e) => {
                    eprintln!("error: cannot run cargo: {e}");
                    1
                }
            }
        }
        Route::Installer(dir) => run_installer(dir, latest),
    }
}

#[cfg(not(windows))]
fn run_installer(dir: &Path, latest: &str) -> i32 {
    let url = format!("https://raw.githubusercontent.com/{REPO}/main/install.sh");
    println!("cli: curl -fsSL {url} | sh   (into {})", dir.display());
    let script = format!("curl -fsSL '{url}' | sh");
    match run_status(
        Command::new("sh")
            .args(["-c", &script])
            .env("CHATGPT_USE_INSTALL_DIR", dir)
            .env("CHATGPT_USE_VERSION", format!("v{latest}")),
    ) {
        Ok(true) => 0,
        Ok(false) => 2,
        Err(e) => {
            eprintln!("error: cannot run the installer: {e}");
            1
        }
    }
}

#[cfg(windows)]
fn run_installer(dir: &Path, _latest: &str) -> i32 {
    // Windows cannot overwrite a running .exe but can rename it, so move this
    // one aside for install.ps1 and put it back if the install fails.
    let exe = dir.join("chatgpt-use.exe");
    let old = dir.join("chatgpt-use.exe.old");
    let _ = std::fs::remove_file(&old);
    let moved = std::fs::rename(&exe, &old).is_ok();
    let url = format!("https://raw.githubusercontent.com/{REPO}/main/install.ps1");
    println!("cli: irm {url} | iex   (into {})", dir.display());
    let ok = run_status(
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &format!("irm '{url}' | iex"),
            ])
            .env("CGU_BIN_DIR", dir),
    );
    if !matches!(ok, Ok(true)) && moved && !exe.exists() {
        let _ = std::fs::rename(&old, &exe);
    }
    match ok {
        Ok(true) => 0,
        Ok(false) => 2,
        Err(e) => {
            eprintln!("error: cannot run the installer: {e}");
            1
        }
    }
}

/// Step 2: refresh one installed copy of the skill.
fn refresh_skill(s: &SkillInstall) {
    let label = format!("skill [{}] {}:", s.channel, s.path);
    match s.channel.as_str() {
        "claude-plugin" => {
            let key = s
                .update
                .trim_start_matches("claude plugin update ")
                .to_string();
            match Command::new("claude")
                .args(["plugin", "update", &key])
                .output()
            {
                Ok(o) if o.status.success() => println!("{label} updated"),
                Ok(o) => println!(
                    "{label} `{}` failed: {}",
                    s.update,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(_) => println!("{label} run: {}", s.update),
            }
        }
        "git" => match Command::new("git")
            .args(["-C", &s.path, "pull", "--ff-only"])
            .output()
        {
            Ok(o) if o.status.success() => println!("{label} pulled"),
            Ok(o) => println!(
                "{label} not updated (git pull --ff-only failed: {})",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => println!("{label} not updated (cannot run git: {e})"),
        },
        _ => println!("{label} run: {}", s.update),
    }
}

/// Plugin installs from `~/.claude/plugins/installed_plugins.json`: keys
/// starting `chatgpt-use@`.
pub fn plugin_installs(home: &Path) -> Vec<SkillInstall> {
    let registry = home
        .join(".claude")
        .join("plugins")
        .join("installed_plugins.json");
    let Ok(body) = std::fs::read_to_string(&registry) else {
        return vec![];
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) else {
        return vec![];
    };
    let plugins = match data.get("plugins") {
        Some(p) if p.is_object() => p,
        _ => &data,
    };
    let Some(map) = plugins.as_object() else {
        return vec![];
    };
    let prefix = format!("{NAME}@");
    map.iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(key, entries)| {
            let entry = if entries.is_array() {
                &entries[0]
            } else {
                entries
            };
            let path = entry["installPath"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| registry.display().to_string());
            SkillInstall {
                channel: "claude-plugin".into(),
                path,
                update: format!("claude plugin update {key}"),
            }
        })
        .collect()
}

/// Root of the git work tree containing `dir`, if any.
fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (!s.is_empty())
        .then(|| PathBuf::from(s))
        .and_then(|p| p.canonicalize().ok())
}

/// Every installed copy of the skill: the Claude Code plugin, plus
/// `~/.agents/skills`, `~/.claude/skills` and `~/.codex/skills` entries that
/// are a git clone (the folder IS the clone's root — a copy that merely sits
/// inside some other repo, like a dotfiles repo, is never pulled) or a
/// copied folder with a SKILL.md.
pub fn find_skill_installs(home: &Path) -> Vec<SkillInstall> {
    find_skill_installs_with(home, &git_toplevel)
}

pub fn find_skill_installs_with(
    home: &Path,
    toplevel: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Vec<SkillInstall> {
    let mut found = plugin_installs(home);
    let mut seen: Vec<PathBuf> = vec![];
    for root in [".agents", ".claude", ".codex"] {
        let Ok(real) = home.join(root).join("skills").join(NAME).canonicalize() else {
            continue;
        };
        if !real.is_dir() || seen.contains(&real) {
            continue;
        }
        seen.push(real.clone());
        let shown = real.display().to_string();
        if toplevel(&real).as_deref() == Some(real.as_path()) {
            found.push(SkillInstall {
                channel: "git".into(),
                update: format!("git -C {shown} pull --ff-only"),
                path: shown,
            });
        } else if real.join("SKILL.md").is_file() {
            found.push(SkillInstall {
                channel: "copy".into(),
                update: format!("npx skills update {NAME}"),
                path: shown,
            });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_home(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "chatgpt-use-upgrade-test-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn status_lines() {
        assert_eq!(status_line("0.0.4", "0.0.5"), "chatgpt-use 0.0.4 -> 0.0.5");
        assert_eq!(
            status_line("0.0.4", "0.0.4"),
            "chatgpt-use 0.0.4 is up to date"
        );
    }

    #[test]
    fn json_shape() {
        let skills = vec![SkillInstall {
            channel: "claude-plugin".into(),
            path: "/p".into(),
            update: "claude plugin update chatgpt-use@leeguooooo-plugins".into(),
        }];
        let r = Report {
            name: NAME,
            current: "0.0.4",
            latest: Some("0.0.5"),
            update_available: true,
            skills: &skills,
            error: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "name": "chatgpt-use", "current": "0.0.4", "latest": "0.0.5", "update_available": true,
                "skills": [{"channel": "claude-plugin", "path": "/p",
                            "update": "claude plugin update chatgpt-use@leeguooooo-plugins"}]
            })
        );
        let r = Report {
            name: NAME,
            current: "0.0.4",
            latest: None,
            update_available: false,
            skills: &[],
            error: Some("offline"),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(v["latest"], serde_json::Value::Null);
        assert_eq!(v["error"], "offline");
    }

    #[test]
    fn routes() {
        assert_eq!(
            route_for(Path::new("/home/u/.cargo/bin/chatgpt-use")),
            Route::Cargo
        );
        assert_eq!(
            route_for(Path::new("/src/chatgpt-use/target/release/chatgpt-use")),
            Route::SourceBuild(PathBuf::from("/src/chatgpt-use/target/release"))
        );
        assert_eq!(
            route_for(Path::new("/home/u/.local/bin/chatgpt-use")),
            Route::Installer(PathBuf::from("/home/u/.local/bin"))
        );
    }

    #[test]
    fn finds_plugin_copy_and_clone() {
        let home = temp_home("find");
        let plugins = home.join(".claude/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("installed_plugins.json"),
            r#"{"version":2,"plugins":{
                "chatgpt-use@leeguooooo-plugins":[{"installPath":"/cache/chatgpt-use/0.0.4"}],
                "chrome-use@leeguooooo-plugins":[{"installPath":"/cache/chrome-use"}]}}"#,
        )
        .unwrap();
        let copy = home.join(".agents/skills/chatgpt-use");
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(copy.join("SKILL.md"), "---\nname: chatgpt-use\n---\n").unwrap();
        let clone = home.join("src/chatgpt-use");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::create_dir_all(home.join(".codex/skills")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&clone, home.join(".codex/skills/chatgpt-use")).unwrap();
        #[cfg(not(unix))]
        std::fs::create_dir_all(home.join(".codex/skills/chatgpt-use")).unwrap();
        let clone_real = home
            .join(".codex/skills/chatgpt-use")
            .canonicalize()
            .unwrap();

        let cr = clone_real.clone();
        let found = find_skill_installs_with(&home, &move |p: &Path| (p == cr).then(|| cr.clone()));
        let copy_real = copy.canonicalize().unwrap();
        assert_eq!(
            found,
            vec![
                SkillInstall {
                    channel: "claude-plugin".into(),
                    path: "/cache/chatgpt-use/0.0.4".into(),
                    update: "claude plugin update chatgpt-use@leeguooooo-plugins".into()
                },
                SkillInstall {
                    channel: "copy".into(),
                    path: copy_real.display().to_string(),
                    update: "npx skills update chatgpt-use".into()
                },
                SkillInstall {
                    channel: "git".into(),
                    path: clone_real.display().to_string(),
                    update: format!("git -C {} pull --ff-only", clone_real.display())
                },
            ]
        );
    }

    #[test]
    fn copy_inside_another_repo_is_not_pulled() {
        let home = temp_home("nested");
        let copy = home.join(".claude/skills/chatgpt-use");
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(copy.join("SKILL.md"), "x").unwrap();
        let h = home.clone();
        let found = find_skill_installs_with(&home, &move |_p: &Path| Some(h.clone()));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].channel, "copy");
    }

    #[test]
    fn nothing_installed_is_empty() {
        let home = temp_home("empty");
        assert!(find_skill_installs_with(&home, &|_p: &Path| None).is_empty());
    }
}
