//! `chatgpt-use review` — code review by ChatGPT that explores the repository
//! itself.
//!
//! A diff alone cannot show a bug: confirming one means reading the callers,
//! definitions and tests around the change. So the reviewer gets the change
//! as a starting point and the read-only tools of `run` (plus `bash`, for
//! `git diff`, `grep -n`, `sed -n`), and chooses what to read.
//!
//! It works on a throwaway `git worktree` of HEAD with the uncommitted and
//! untracked work copied in, never on the caller's checkout: `safe`
//! permissions block destructive and network commands but do not make a shell
//! read-only, and a review must not be able to change the code it reviews.
//! The worktree is removed afterwards unless `--keep`.
//!
//! The change is pinned to a literal merge-base commit in the prompt, because
//! `safe` mode refuses `$(…)` substitution.

use crate::channel::{Channel, ChannelOptions};
use crate::cli::{PermissionMode, ReviewArgs};
use crate::tools;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(args: &ReviewArgs) -> Result<()> {
    let here = std::env::current_dir()?;
    let dir = std::env::temp_dir().join(format!("chatgpt-use-review-{}", std::process::id()));
    let ws = Workspace::prepare(&here, &args.base, &dir)?;
    eprintln!(
        "reviewing {} change(s) since {} ({}) in {}",
        ws.changed_files,
        args.base,
        &ws.merge_base[..12.min(ws.merge_base.len())],
        ws.dir.display()
    );

    let specs: Vec<_> = tools::builtin_specs()
        .into_iter()
        .filter(|s| tools::is_read_only(&s.name) || s.name == "bash")
        .collect();
    let task = review_task(&args.base, &ws.merge_base, args.focus.as_deref());

    let opts = ChannelOptions {
        profile: args.channel.profile.clone(),
        session: args.channel.session.clone(),
        project: args.channel.project.clone(),
        timeout_secs: args.channel.timeout,
        model: args.channel.model.clone(),
        busy_fail: args.channel.busy == crate::cli::BusyPolicy::Fail,
        receipt: None,
        ignore_cooldown: false,
    };
    let report = Channel::connect(&opts).and_then(|mut channel| {
        let report = crate::cmd::run::agent_loop(
            &mut channel,
            &specs,
            &task,
            args.max_steps,
            true,
            PermissionMode::Safe,
            &ws.dir,
        );
        channel.close();
        report
    });

    if args.keep {
        eprintln!("kept the review worktree at {}", ws.dir.display());
    } else {
        ws.remove();
    }
    let report = report?;
    crate::ledger::record(
        "review",
        serde_json::json!({"base": args.base, "files": ws.changed_files, "report_chars": report.len()}),
    );
    println!("{report}");
    Ok(())
}

/// What the reviewer is asked to do.
fn review_task(base: &str, merge_base: &str, focus: Option<&str>) -> String {
    let mut task = format!(
        "Code review. The change under review is everything from commit {merge_base} (where \
         this branch left {base}) to the current working tree, including new files. Start with \
         `git diff {merge_base} --stat`, then `git diff {merge_base}`. Then read whatever else you \
         need to judge the change for real — callers, definitions, related tests, strings of code \
         the change builds — and choose for yourself what to read. Large files come in windows: \
         use `grep -n` to find the lines you need and read_file with offset/limit around them. \
         Look for real bugs: wrong logic, edge cases that break, error paths that leave state \
         wrong, races, resource leaks, security problems. Do not modify any file, and do not run \
         builds or test suites: passing tests are not a review, and the point is to find what \
         they miss by reading the code. Before you answer, read the changed code itself, not \
         only the diff.\n\n\
         Answer in exactly this shape:\n\
         FINDINGS — each verified issue, most severe first: file:line, what goes wrong, a \
         concrete failure scenario, a suggested fix. Write \"none\" if you found none.\n\
         EXAMINED — the files and functions you actually read to reach that verdict.\n\
         NOT CHECKED — parts of the change you did not get to."
    );
    if let Some(f) = focus.filter(|f| !f.trim().is_empty()) {
        task.push_str(&format!("\n\nFocus: {f}"));
    }
    task
}

/// The throwaway copy of the change that the reviewer works in.
struct Workspace {
    repo: PathBuf,
    dir: PathBuf,
    merge_base: String,
    changed_files: usize,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?}"))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Workspace {
    /// A detached worktree of HEAD at `dir`, with the working tree's
    /// uncommitted changes applied and its untracked files copied in (and
    /// marked intent-to-add, so `git diff` shows them).
    fn prepare(cwd: &Path, base: &str, dir: &Path) -> Result<Self> {
        let repo = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])
            .context("chatgpt-use review must run inside a git repository")?
            .trim());
        let merge_base = git(&repo, &["merge-base", base, "HEAD"])
            .with_context(|| format!("no merge-base with {base:?}; pass --base <branch or commit>"))?
            .trim()
            .to_string();
        let _ = std::fs::remove_dir_all(dir);
        git(&repo, &["worktree", "add", "--detach", &dir.to_string_lossy(), "HEAD"])?;
        let ws = Workspace { repo: repo.clone(), dir: dir.to_path_buf(), merge_base, changed_files: 0 };

        let copy = || -> Result<()> {
            let patch = git(&repo, &["diff", "HEAD", "--binary"])?;
            if !patch.is_empty() {
                let file = dir.join(".chatgpt-use-review.patch");
                std::fs::write(&file, patch)?;
                git(dir, &["apply", "--whitespace=nowarn", &file.to_string_lossy()])?;
                std::fs::remove_file(&file)?;
            }
            let untracked = git(&repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
            for rel in untracked.split('\0').filter(|p| !p.is_empty()) {
                let to = dir.join(rel);
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(repo.join(rel), &to).with_context(|| format!("copying {rel}"))?;
                git(dir, &["add", "--intent-to-add", "--", rel])?;
            }
            Ok(())
        };
        if let Err(e) = copy() {
            ws.remove();
            return Err(e.context("copying the working tree's changes into the review worktree"));
        }
        let changed = git(dir, &["diff", &ws.merge_base, "--name-only"])?;
        Ok(Workspace { changed_files: changed.lines().count(), ..ws })
    }

    fn remove(&self) {
        let _ = git(&self.repo, &["worktree", "remove", "--force", &self.dir.to_string_lossy()]);
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = git(&self.repo, &["worktree", "prune"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn the_review_worktree_holds_the_whole_change_and_goes_away() {
        let root = std::env::temp_dir().join(format!("cgu-review-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "base"]);
        sh(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        sh(&repo, &["commit", "-q", "-am", "committed change"]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\nthree uncommitted\n").unwrap();
        std::fs::create_dir_all(repo.join("new")).unwrap();
        std::fs::write(repo.join("new/b.txt"), "untracked\n").unwrap();

        let dir = root.join("wt");
        let ws = Workspace::prepare(&repo, "main", &dir).unwrap();
        let main = git(&repo, &["rev-parse", "main"]).unwrap();
        assert_eq!(ws.merge_base, main.trim());
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\ntwo\nthree uncommitted\n");
        assert_eq!(std::fs::read_to_string(dir.join("new/b.txt")).unwrap(), "untracked\n");
        let diff = git(&dir, &["diff", &ws.merge_base, "--name-only"]).unwrap();
        assert!(diff.contains("a.txt") && diff.contains("new/b.txt"), "{diff}");
        assert_eq!(ws.changed_files, 2);
        // The caller's checkout is untouched.
        assert_eq!(git(&repo, &["status", "--short"]).unwrap().lines().count(), 2);

        ws.remove();
        assert!(!dir.exists());
        assert!(!git(&repo, &["worktree", "list"]).unwrap().contains("wt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_base_says_how_to_fix_it() {
        let root = std::env::temp_dir().join(format!("cgu-review-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        sh(&root, &["init", "-q", "-b", "trunk"]);
        std::fs::write(root.join("x"), "x").unwrap();
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-q", "-m", "x"]);
        let e = match Workspace::prepare(&root, "main", &root.join("wt")) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("main does not exist here"),
        };
        assert!(e.contains("--base"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_task_pins_a_literal_commit_and_forbids_edits() {
        let t = review_task("main", "abc123", Some("the cancel path"));
        assert!(t.contains("git diff abc123") && !t.contains("$("), "safe mode refuses $(…)");
        assert!(t.contains("Do not modify any file"));
        assert!(t.contains("do not run builds or test suites"), "tests passing is not a review");
        for section in ["FINDINGS", "EXAMINED", "NOT CHECKED"] {
            assert!(t.contains(section), "{section}");
        }
        assert!(t.ends_with("Focus: the cancel path"));
        assert!(!review_task("main", "abc", None).contains("Focus:"));
    }

    #[test]
    fn the_reviewer_gets_read_only_tools_plus_bash() {
        let names: Vec<String> = tools::builtin_specs()
            .into_iter()
            .filter(|s| tools::is_read_only(&s.name) || s.name == "bash")
            .map(|s| s.name)
            .collect();
        assert!(names.iter().any(|n| n == "read_file") && names.iter().any(|n| n == "bash"));
        assert!(!names.iter().any(|n| n == "write_file" || n == "edit_file"), "{names:?}");
    }
}
