//! OAuth 2.1 Authorization-Code + PKCE (S256) provider for the MCP server.
//!
//! Designed for a single-operator setup: the "resource owner password" is the
//! existing MCP shared secret (printed at startup as "[mcp] oauth password: …").
//! A browser form collects that password during the Authorization step, so
//! ChatGPT's OAuth connector can complete the full Authorization-Code + PKCE
//! dance without a separate identity system.
//!
//! State is held in a `static Mutex<OauthState>` (initialized via `OnceLock`).
//! All issued codes / tokens are in-process; they do not survive a server restart.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// In-memory state
// ---------------------------------------------------------------------------

/// Metadata stored for an issued authorization code.
struct AuthCodeEntry {
    code_challenge: String,
    redirect_uri: String,
    expiry: Instant,
}

/// Metadata stored for an issued access token.
struct TokenEntry {
    expiry: Instant,
}

struct OauthState {
    codes: HashMap<String, AuthCodeEntry>,
    tokens: HashMap<String, TokenEntry>,
}

impl OauthState {
    fn new() -> Self {
        OauthState {
            codes: HashMap::new(),
            tokens: HashMap::new(),
        }
    }
}

fn state() -> &'static Mutex<OauthState> {
    static INSTANCE: OnceLock<Mutex<OauthState>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(OauthState::new()))
}

// ---------------------------------------------------------------------------
// Random helpers (no RNG crate — /dev/urandom only)
// ---------------------------------------------------------------------------

/// Read `n` random bytes from /dev/urandom and return them.
fn random_bytes(n: usize) -> Vec<u8> {
    let mut f = std::fs::File::open("/dev/urandom").expect("/dev/urandom must be available");
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf).expect("reading /dev/urandom");
    buf
}

/// Generate a random URL-safe-base64 (no-pad) token from `n` raw bytes.
fn random_token(n: usize) -> String {
    URL_SAFE_NO_PAD.encode(random_bytes(n))
}

/// Generate a random hex string from `n` raw bytes.
fn random_hex(n: usize) -> String {
    random_bytes(n).iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// PKCE helpers
// ---------------------------------------------------------------------------

/// Compute `BASE64URL-NOPAD(SHA256(ascii_verifier))` — the S256 code challenge
/// computed from a code verifier.
pub fn pkce_s256_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    URL_SAFE_NO_PAD.encode(digest)
}

// ---------------------------------------------------------------------------
// URL-encoded form parser (no extra crate)
// ---------------------------------------------------------------------------

/// Decode a percent-encoded URL component (`%XX` → byte, `+` → space).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex_str) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte_val) = u8::from_str_radix(hex_str, 16) {
                    out.push(byte_val);
                    i += 3;
                    continue;
                }
            }
            // Not a valid %XX sequence — pass through literally.
            out.push(bytes[i]);
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse an `application/x-www-form-urlencoded` body (or query string) into
/// a `HashMap<String, String>`. Keys and values are percent-decoded.
pub fn parse_urlencoded(input: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in input.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        };
        map.insert(k, v);
    }
    map
}

// ---------------------------------------------------------------------------
// RFC 8414 — OAuth Authorization Server Metadata
// ---------------------------------------------------------------------------

/// Build the `/.well-known/oauth-authorization-server` discovery document.
pub fn discovery_document(issuer: &str) -> serde_json::Value {
    serde_json::json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "registration_endpoint": format!("{issuer}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["mcp"]
    })
}

// ---------------------------------------------------------------------------
// RFC 9728 — OAuth Protected Resource Metadata
// ---------------------------------------------------------------------------

/// Build the `/.well-known/oauth-protected-resource` document.
pub fn protected_resource_document(issuer: &str) -> serde_json::Value {
    serde_json::json!({
        "resource": issuer,
        "authorization_servers": [issuer]
    })
}

// ---------------------------------------------------------------------------
// Dynamic Client Registration (RFC 7591)
// ---------------------------------------------------------------------------

/// Handle `POST /register` — accept any client, return a generated client_id.
/// We accept any non-empty request body; the caller can send {} or a full RFC
/// 7591 document.
pub fn register() -> serde_json::Value {
    let client_id = format!("cgu-client-{}", random_hex(8));
    serde_json::json!({
        "client_id": client_id,
        "token_endpoint_auth_method": "none",
        "grant_types": ["authorization_code"],
        "response_types": ["code"]
    })
}

// ---------------------------------------------------------------------------
// Authorization endpoint
// ---------------------------------------------------------------------------

/// Return the HTML page for `GET /authorize?…` — a password form that re-posts
/// all existing query parameters as hidden fields alongside a `password` input.
pub fn authorize_form_html(query: &str) -> String {
    let params = parse_urlencoded(query);
    let mut hidden = String::new();
    for (k, v) in &params {
        // Escape for HTML attribute context.
        let k_esc = k.replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;");
        let v_esc = v.replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;");
        hidden.push_str(&format!(
            r#"<input type="hidden" name="{k_esc}" value="{v_esc}">"#
        ));
        hidden.push('\n');
    }

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>MCP Server — Authorization</title>
<style>
  body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
         background:#f5f5f5; display:flex; align-items:center; justify-content:center;
         min-height:100vh; margin:0; }}
  .card {{ background:#fff; border-radius:8px; padding:2rem 2.5rem;
           box-shadow:0 2px 12px rgba(0,0,0,.1); max-width:380px; width:100%; }}
  h1 {{ font-size:1.2rem; margin:0 0 1.2rem; color:#111; }}
  label {{ display:block; font-size:.85rem; color:#555; margin-bottom:.3rem; }}
  input[type=password] {{ width:100%; padding:.55rem .75rem; border:1px solid #ccc;
                          border-radius:5px; font-size:1rem; box-sizing:border-box; }}
  button {{ margin-top:1rem; width:100%; padding:.6rem; background:#0070f3; color:#fff;
            border:none; border-radius:5px; font-size:1rem; cursor:pointer; }}
  button:hover {{ background:#0060df; }}
  p.hint {{ font-size:.8rem; color:#888; margin-top:.8rem; }}
</style>
</head>
<body>
<div class="card">
  <h1>MCP Server Authorization</h1>
  <form method="POST" action="/authorize">
    {hidden}
    <label for="pw">Server password</label>
    <input type="password" id="pw" name="password" autofocus required>
    <button type="submit">Authorize</button>
  </form>
  <p class="hint">Enter the token shown at server startup.</p>
</div>
</body>
</html>"#,
        hidden = hidden
    )
}

/// Handle `POST /authorize` (form submission).
///
/// `params` is a merged map of query string + form body fields.
/// On success, returns the redirect URL (`302 Location` target).
/// On failure, returns an error message for the `400` response.
pub fn authorize_submit(
    params: &HashMap<String, String>,
    server_password: &str,
) -> Result<String, String> {
    // Verify password.
    let pw = params.get("password").map(|s| s.as_str()).unwrap_or("");
    if pw != server_password {
        return Err("invalid password".to_string());
    }

    // PKCE: only S256 is supported.
    let method = params
        .get("code_challenge_method")
        .map(|s| s.as_str())
        .unwrap_or("");
    if method != "S256" {
        return Err(format!(
            "unsupported code_challenge_method: {method:?} (only S256 is supported)"
        ));
    }

    let code_challenge = params
        .get("code_challenge")
        .ok_or_else(|| "missing code_challenge".to_string())?
        .clone();

    let redirect_uri = params
        .get("redirect_uri")
        .ok_or_else(|| "missing redirect_uri".to_string())?
        .clone();

    let state_val = params.get("state").cloned().unwrap_or_default();

    // Generate and store the authorization code (10 minute TTL).
    let code = random_token(24);
    {
        let mut st = state().lock().unwrap();
        // Prune expired codes opportunistically.
        let now = Instant::now();
        st.codes.retain(|_, e| e.expiry > now);
        st.codes.insert(
            code.clone(),
            AuthCodeEntry {
                code_challenge,
                redirect_uri: redirect_uri.clone(),
                expiry: now + Duration::from_secs(600),
            },
        );
    }

    // Build redirect URL.
    let mut location = format!("{redirect_uri}?code={code}");
    if !state_val.is_empty() {
        location.push_str(&format!("&state={state_val}"));
    }
    Ok(location)
}

// ---------------------------------------------------------------------------
// Token endpoint
// ---------------------------------------------------------------------------

/// Handle `POST /token` — exchange an authorization code for an access token.
///
/// `params` is the parsed form (or JSON) body.
/// Returns the token JSON on success, or an error string on failure.
pub fn exchange_token(
    params: &HashMap<String, String>,
) -> Result<serde_json::Value, String> {
    let grant_type = params
        .get("grant_type")
        .map(|s| s.as_str())
        .unwrap_or("");
    if grant_type != "authorization_code" {
        return Err(format!("unsupported grant_type: {grant_type:?}"));
    }

    let code = params
        .get("code")
        .ok_or_else(|| "missing code".to_string())?
        .clone();

    let verifier = params
        .get("code_verifier")
        .ok_or_else(|| "missing code_verifier".to_string())?
        .clone();

    let redirect_uri = params
        .get("redirect_uri")
        .ok_or_else(|| "missing redirect_uri".to_string())?
        .clone();

    let entry = {
        let mut st = state().lock().unwrap();
        let now = Instant::now();
        // Remove expired codes while we're here.
        st.codes.retain(|_, e| e.expiry > now);
        st.codes
            .remove(&code)
            .ok_or_else(|| "unknown or expired authorization code".to_string())?
    };

    // Verify redirect_uri matches.
    if entry.redirect_uri != redirect_uri {
        return Err("redirect_uri mismatch".to_string());
    }

    // PKCE S256 verification.
    let computed_challenge = pkce_s256_challenge(&verifier);
    if computed_challenge != entry.code_challenge {
        return Err("PKCE verification failed: code_verifier does not match code_challenge".to_string());
    }

    // Mint access token (1 hour TTL).
    let access_token = random_token(24);
    {
        let mut st = state().lock().unwrap();
        let now = Instant::now();
        // Prune expired tokens opportunistically.
        st.tokens.retain(|_, e| e.expiry > now);
        st.tokens.insert(
            access_token.clone(),
            TokenEntry {
                expiry: now + Duration::from_secs(3600),
            },
        );
    }

    Ok(serde_json::json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": "mcp"
    }))
}

// ---------------------------------------------------------------------------
// Bearer token validation
// ---------------------------------------------------------------------------

/// Returns `true` if `token` is a currently valid (non-expired) access token.
pub fn validate_bearer(token: &str) -> bool {
    let st = state().lock().unwrap();
    match st.tokens.get(token) {
        Some(entry) => entry.expiry > Instant::now(),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // (a) PKCE S256 — RFC 7636 test vector.
    // verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
    // expected challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    #[test]
    fn pkce_s256_rfc7636_test_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce_s256_challenge(verifier), expected);
    }

    // (b) discovery_document contains the required fields/endpoints.
    #[test]
    fn discovery_document_has_required_fields() {
        let doc = discovery_document("https://example.com");
        assert_eq!(doc["issuer"], "https://example.com");
        assert_eq!(doc["authorization_endpoint"], "https://example.com/authorize");
        assert_eq!(doc["token_endpoint"], "https://example.com/token");
        assert_eq!(doc["registration_endpoint"], "https://example.com/register");

        let grant_types = doc["grant_types_supported"].as_array().unwrap();
        assert!(grant_types.iter().any(|v| v == "authorization_code"));

        let methods = doc["code_challenge_methods_supported"].as_array().unwrap();
        assert!(methods.iter().any(|v| v == "S256"));

        let response_types = doc["response_types_supported"].as_array().unwrap();
        assert!(response_types.iter().any(|v| v == "code"));
    }

    // (c) Full authorize → token roundtrip: correct verifier succeeds; wrong fails.
    #[test]
    fn full_authorize_token_roundtrip() {
        // Build a PKCE pair.
        let verifier = "test_code_verifier_for_roundtrip_test_abc123xyz";
        let challenge = pkce_s256_challenge(verifier);

        let redirect_uri = "https://client.example.com/callback";
        let state_val = "random_state_value";

        // -- Authorize step --
        let mut auth_params = HashMap::new();
        auth_params.insert("code_challenge".to_string(), challenge.clone());
        auth_params.insert("code_challenge_method".to_string(), "S256".to_string());
        auth_params.insert("redirect_uri".to_string(), redirect_uri.to_string());
        auth_params.insert("state".to_string(), state_val.to_string());
        auth_params.insert("password".to_string(), "correct_password".to_string());

        let location = authorize_submit(&auth_params, "correct_password")
            .expect("authorize_submit should succeed with correct password");

        // Extract the code from the redirect URL.
        assert!(location.starts_with(redirect_uri), "redirect should point to redirect_uri");
        let code = location
            .split('?')
            .nth(1)
            .and_then(|q| q.split('&').find(|p| p.starts_with("code=")))
            .and_then(|p| p.strip_prefix("code="))
            .expect("redirect URL must contain code=…")
            .to_string();

        assert!(location.contains(&format!("state={state_val}")), "state must be preserved");

        // -- Token exchange step: correct verifier --
        let mut token_params = HashMap::new();
        token_params.insert("grant_type".to_string(), "authorization_code".to_string());
        token_params.insert("code".to_string(), code.clone());
        token_params.insert("code_verifier".to_string(), verifier.to_string());
        token_params.insert("redirect_uri".to_string(), redirect_uri.to_string());

        let token_resp = exchange_token(&token_params).expect("token exchange should succeed");
        assert_eq!(token_resp["token_type"], "Bearer");
        let access_token = token_resp["access_token"].as_str().unwrap();
        assert!(!access_token.is_empty());
        assert!(validate_bearer(access_token), "issued token must be valid");

        // -- Replay: code is consumed; second use must fail --
        let mut replay_params = HashMap::new();
        replay_params.insert("grant_type".to_string(), "authorization_code".to_string());
        replay_params.insert("code".to_string(), code);
        replay_params.insert("code_verifier".to_string(), verifier.to_string());
        replay_params.insert("redirect_uri".to_string(), redirect_uri.to_string());
        assert!(
            exchange_token(&replay_params).is_err(),
            "replayed code must be rejected"
        );
    }

    #[test]
    fn token_exchange_fails_with_wrong_verifier() {
        let verifier = "correct_verifier_abcdefg12345";
        let challenge = pkce_s256_challenge(verifier);
        let redirect_uri = "https://client.example.com/cb";

        let mut auth_params = HashMap::new();
        auth_params.insert("code_challenge".to_string(), challenge);
        auth_params.insert("code_challenge_method".to_string(), "S256".to_string());
        auth_params.insert("redirect_uri".to_string(), redirect_uri.to_string());
        auth_params.insert("password".to_string(), "secret".to_string());

        let location = authorize_submit(&auth_params, "secret").unwrap();
        let code = location
            .split('?')
            .nth(1)
            .and_then(|q| q.split('&').find(|p| p.starts_with("code=")))
            .and_then(|p| p.strip_prefix("code="))
            .unwrap()
            .to_string();

        let mut token_params = HashMap::new();
        token_params.insert("grant_type".to_string(), "authorization_code".to_string());
        token_params.insert("code".to_string(), code);
        token_params.insert("code_verifier".to_string(), "WRONG_verifier".to_string());
        token_params.insert("redirect_uri".to_string(), redirect_uri.to_string());

        let err = exchange_token(&token_params);
        assert!(err.is_err(), "wrong verifier must be rejected");
        let msg = err.unwrap_err();
        assert!(msg.contains("PKCE"), "error should mention PKCE: {msg}");
    }

    #[test]
    fn authorize_submit_rejects_wrong_password() {
        let mut params = HashMap::new();
        params.insert("code_challenge".to_string(), "x".to_string());
        params.insert("code_challenge_method".to_string(), "S256".to_string());
        params.insert("redirect_uri".to_string(), "https://example.com/cb".to_string());
        params.insert("password".to_string(), "wrong".to_string());

        let err = authorize_submit(&params, "correct");
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("invalid password"));
    }

    #[test]
    fn percent_decode_handles_plus_and_hex() {
        assert_eq!(percent_decode("hello+world"), "hello world");
        assert_eq!(percent_decode("foo%3Dbar"), "foo=bar");
        assert_eq!(percent_decode("a%20b%20c"), "a b c");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn parse_urlencoded_splits_correctly() {
        let m = parse_urlencoded("foo=bar&baz=qux&empty=");
        assert_eq!(m.get("foo").map(|s| s.as_str()), Some("bar"));
        assert_eq!(m.get("baz").map(|s| s.as_str()), Some("qux"));
        assert_eq!(m.get("empty").map(|s| s.as_str()), Some(""));
    }

    #[test]
    fn validate_bearer_rejects_unknown_token() {
        assert!(!validate_bearer("not_a_real_token"));
    }
}
