//! Small platform adapters used by the CLI.
use std::path::PathBuf;

pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    { std::env::var_os("USERPROFILE").map(PathBuf::from).or_else(|| Some(PathBuf::from(std::env::var_os("HOMEDRIVE")?).join(std::env::var_os("HOMEPATH")?))) }
    #[cfg(not(windows))]
    { std::env::var_os("HOME").map(PathBuf::from) }
}

pub fn path_separator() -> char { if cfg!(windows) { ';' } else { ':' } }
