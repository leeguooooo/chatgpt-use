//! `chatgpt-use init` — one-time setup. Generates a random auth token, stores it
//! at `~/.chatgpt-use/auth.json`, and prints the next steps. Borrowed from
//! devspace's init/auth-file UX (cleaner than passing `--token` by hand). The
//! `mcp` command auto-loads this token when `--token` is omitted.

use crate::cli::InitArgs;
use anyhow::{Context, Result};
use std::io::Read;

/// Path to the auth file: `~/.chatgpt-use/auth.json`.
pub fn auth_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".chatgpt-use").join("auth.json")
}

/// Load the saved token, if any (used by `mcp` when --token is omitted).
pub fn load_token() -> Option<String> {
    let body = std::fs::read_to_string(auth_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v.get("token")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

/// 16 random bytes from /dev/urandom, hex-encoded (no RNG crate needed).
fn random_token() -> Result<String> {
    let mut f = std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?;
    let mut buf = [0u8; 16];
    f.read_exact(&mut buf).context("reading random bytes")?;
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("cgu-{hex}"))
}

pub fn run(args: &InitArgs) -> Result<()> {
    let path = auth_path();
    if path.exists() && !args.force {
        if let Some(tok) = load_token() {
            println!("auth token already exists at {}", path.display());
            println!("token: {tok}");
            println!("(re-run with --force to regenerate)");
            return Ok(());
        }
    }

    let token = random_token()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating ~/.chatgpt-use")?;
    }
    let body = serde_json::json!({ "token": token, "v": 1 });
    std::fs::write(&path, serde_json::to_string_pretty(&body)?)
        .with_context(|| format!("writing {}", path.display()))?;
    // tighten perms (owner read/write only) — best-effort.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    println!("✓ wrote auth token to {}", path.display());
    println!("  token: {token}");
    println!();
    println!("Next steps:");
    println!("  1. Start the MCP server (auto-loads the token):");
    println!("       chatgpt-use mcp --port 8788 --cwd <your project>");
    println!("  2. Expose it over a tunnel, e.g.:");
    println!("       cloudflared tunnel --url http://127.0.0.1:8788");
    println!("  3. In ChatGPT → Settings → Apps → Add custom connector, use the");
    println!("     public URL with the token, e.g.  https://<host>/?token={token}");
    println!("     (or header  Authorization: Bearer {token}), No-Auth mode.");
    println!("  Note: read-only profile by default; pass --profile full for write/bash (trusted only).");
    Ok(())
}
