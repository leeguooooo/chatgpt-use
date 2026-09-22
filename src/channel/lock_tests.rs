//! Exercise the real OS lock in separate processes without opening a browser.
//! Environment overrides belong only to the child, so parallel tests do not
//! race over HOME or touch the user's actual shared lock.

use super::*;
use std::process::{Child, Stdio};

struct Worker(Child);

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn check_lock(home_source: &str) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "chatgpt-use-lock-{}-{unique}-{home_source}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".chatgpt-web.lock");
    let held = File::create(&path).unwrap();
    held.lock().unwrap();

    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "channel::lock_tests::lock_child",
            "--ignored",
            "--nocapture",
        ])
        .env("CGU_TEST_LOCK_DIR", &dir)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("HOMEDRIVE")
        .env_remove("HOMEPATH")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    match home_source {
        "home" => {
            command
                .env("HOME", &dir)
                .env("USERPROFILE", dir.join("different-profile"));
        }
        "profile" => {
            command.env("USERPROFILE", &dir);
        }
        "drive-path" => {
            let text = dir.to_str().unwrap();
            assert_eq!(&text[1..2], ":", "expected a drive-qualified temp path");
            command.env("HOMEDRIVE", &text[..2]);
            command.env("HOMEPATH", &text[2..]);
        }
        _ => unreachable!(),
    }
    let mut worker = Worker(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(15);
    while !dir.join("ready").exists() {
        assert!(
            worker.0.try_wait().unwrap().is_none(),
            "child failed before entering the wait test ({home_source})"
        );
        assert!(Instant::now() < deadline, "child did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The child has already verified busy-fail and is now attempting a normal
    // acquisition. It must not enter while this process still owns the lock.
    std::thread::sleep(Duration::from_millis(350));
    assert!(worker.0.try_wait().unwrap().is_none());
    assert!(!dir.join("acquired").exists());
    drop(held);

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = worker.0.try_wait().unwrap() {
            assert!(status.success(), "lock child failed ({home_source})");
            break;
        }
        assert!(Instant::now() < deadline, "child did not acquire released lock");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(dir.join("acquired").exists());
    drop(worker);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn explicit_home_preserves_shared_lock_location() {
    check_lock("home");
}

#[test]
#[cfg(windows)]
fn missing_home_uses_userprofile_and_serializes_processes() {
    check_lock("profile");
}

#[test]
#[cfg(windows)]
fn missing_home_and_userprofile_use_drive_path() {
    check_lock("drive-path");
}

/// Invoked only by check_lock; never calls chrome-use or ChatGPT.
#[test]
#[ignore = "subprocess helper for the surface lock tests"]
fn lock_child() {
    let dir = PathBuf::from(std::env::var_os("CGU_TEST_LOCK_DIR").unwrap());
    assert_eq!(lock_path(), Some(dir.join(".chatgpt-web.lock")));
    let error = match SurfaceLock::acquire(true) {
        Ok(_) => panic!("busy-fail acquired a lock held by another process"),
        Err(error) => error,
    };
    let error = channel_error(&error).expect("expected a typed busy error");
    assert_eq!(error.kind, ErrorKind::Busy);
    assert_eq!(error.submitted, Submitted::No);
    std::fs::write(dir.join("ready"), "ready").unwrap();

    let lock = SurfaceLock::acquire(false).unwrap();
    assert!(lock._file.is_some(), "acquisition silently skipped locking");
    std::fs::write(dir.join("acquired"), "acquired").unwrap();
    drop(lock);
    // Closing a channel releases the lock for the next request.
    assert!(SurfaceLock::acquire(true).unwrap()._file.is_some());
}
