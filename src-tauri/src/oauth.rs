//! In-app authorization-code flow.
//!
//! A desktop app has no public URL, so the redirect target is a loopback
//! listener that lives only for the seconds the browser needs to come back
//! with a code. Ported from `probe/lark_auth.py`, which proved this exact
//! exchange against the live tenant.
//!
//! PKCE is sent when enabled. It does not replace the client secret for Lark
//! (the token endpoint still requires it), but it does stop another local
//! process from racing the loopback redirect and stealing the code.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::auth::TokenSet;
use crate::config::AppConfig;
use crate::error::{CoreError, Result};

const AUTHORIZE_URL: &str = "https://open.larksuite.com/open-apis/authen/v1/authorize";
const TOKEN_URL: &str = "https://open.larksuite.com/open-apis/authen/v2/oauth/token";
pub const SCOPES: &str = "bitable:app contact:user.base:readonly offline_access";

/// How long to wait for the user to finish consenting in the browser.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(180);
/// Per-connection read budget, so a stray socket cannot stall the wait.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

fn random_urlsafe(bytes: usize) -> String {
    // Enough entropy for state/verifier without pulling in a CSPRNG crate:
    // uuid is already a transitive dependency of the Tauri stack.
    let mut raw = Vec::with_capacity(bytes);
    while raw.len() < bytes {
        raw.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    raw.truncate(bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

fn code_challenge_of(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// The URL the user is sent to, plus the secrets needed to finish the exchange.
pub struct Pending {
    pub url: String,
    pub state: String,
    pub verifier: String,
    pub redirect: String,
}

pub fn begin(cfg: &AppConfig, redirect: &str) -> Pending {
    let state = random_urlsafe(24);
    let verifier = random_urlsafe(48);
    let params = [
        ("client_id", cfg.app_id.as_str()),
        ("redirect_uri", redirect),
        ("response_type", "code"),
        ("state", state.as_str()),
        ("scope", SCOPES),
        ("code_challenge", &code_challenge_of(&verifier)),
        ("code_challenge_method", "S256"),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    Pending {
        url: format!("{AUTHORIZE_URL}?{query}"),
        state,
        verifier,
        redirect: redirect.to_string(),
    }
}

/// Minimal fixed response. Nothing from the query string is echoed: any page
/// in the browser can reach this port, so reflecting it would both allow
/// script injection and print the auth code on screen.
fn respond(stream: &mut TcpStream, ok: bool) {
    let message = if ok {
        "Signed in. You can close this tab and return to OMSN."
    } else {
        "Sign-in failed. Return to OMSN and try again."
    };
    let body = format!("<html><body><h3>{message}</h3></body></html>");
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Security-Policy: default-src 'none'\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn query_of(request_line: &str) -> Option<String> {
    // "GET /callback?code=...&state=... HTTP/1.1"
    let target = request_line.split_whitespace().nth(1)?;
    target.split_once('?').map(|(_, q)| q.to_string())
}

fn parse_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((
                urlencoding::decode(k).ok()?.into_owned(),
                urlencoding::decode(v).ok()?.into_owned(),
            ))
        })
        .collect()
}

/// Block until the browser hits the loopback redirect, then return the code.
///
/// Runs on a blocking thread: it owns a synchronous listener and must not be
/// polled on the async runtime.
pub fn wait_for_code(port: u16, expected_state: &str) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
        CoreError::Config(format!(
            "Cannot listen on 127.0.0.1:{port} for the sign-in redirect: {e}"
        ))
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| CoreError::Config(e.to_string()))?;

    let deadline = Instant::now() + CONSENT_TIMEOUT;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    continue;
                }
                let Some(query) = query_of(&line) else {
                    respond(&mut stream, false);
                    continue;
                };
                let pairs = parse_pairs(&query);
                let get = |key: &str| {
                    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
                };

                // A mismatched state means this redirect is not ours.
                if get("state").as_deref() != Some(expected_state) {
                    respond(&mut stream, false);
                    continue;
                }
                match get("code") {
                    Some(code) if !code.is_empty() => {
                        respond(&mut stream, true);
                        return Ok(code);
                    }
                    _ => {
                        respond(&mut stream, false);
                        // Report the error class, never the raw query.
                        return Err(CoreError::Auth(format!(
                            "Lark denied the sign-in ({})",
                            get("error").unwrap_or_else(|| "no code returned".into())
                        )));
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(e) => return Err(CoreError::Config(format!("redirect listener failed: {e}"))),
        }
    }
    Err(CoreError::Auth("Sign-in timed out. Please try again.".into()))
}

/// Trade the authorization code for tokens.
pub async fn exchange(
    http: &reqwest::Client,
    cfg: &AppConfig,
    pending: &Pending,
    code: &str,
    now_secs: i64,
) -> Result<TokenSet> {
    let resp = http
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": cfg.app_id,
            "client_secret": cfg.app_secret,
            "code": code,
            "redirect_uri": pending.redirect,
            "code_verifier": pending.verifier,
        }))
        .send()
        .await?;

    let body: Value = resp.json().await?;
    let lark_code = body.get("code").and_then(Value::as_i64).unwrap_or(0);
    if lark_code != 0 {
        // Report msg only: the body can carry tokens.
        return Err(CoreError::Auth(format!(
            "Sign-in failed: {}",
            body.get("msg").and_then(Value::as_str).unwrap_or("unknown error")
        )));
    }
    TokenSet::from_response(&body, now_secs)
}

/// Loopback port from the configured redirect URI.
pub fn redirect_port(redirect: &str) -> Result<u16> {
    url::Url::parse(redirect)
        .map_err(|e| CoreError::Config(format!("OMSN_OAUTH_REDIRECT is not a URL: {e}")))?
        .port()
        .ok_or_else(|| {
            CoreError::Config("OMSN_OAUTH_REDIRECT must include a port, e.g. :8765".into())
        })
}

/// Reject a redirect that is not loopback.
///
/// The URI is config, and the listener always binds 127.0.0.1 — so a wrong or
/// tampered value would send the authorization code to a remote host while the
/// app waited locally and silently timed out.
pub fn validate_redirect(redirect: &str) -> Result<()> {
    let parsed = url::Url::parse(redirect)
        .map_err(|e| CoreError::Config(format!("OMSN_OAUTH_REDIRECT is not a URL: {e}")))?;
    let host = parsed.host_str().unwrap_or_default();
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
        return Err(CoreError::Config(format!(
            "OMSN_OAUTH_REDIRECT must point at loopback, got host {host:?}"
        )));
    }
    if parsed.scheme() != "http" {
        return Err(CoreError::Config(
            "OMSN_OAUTH_REDIRECT must use http on loopback".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AppConfig {
        AppConfig {
            base_token: "bas".into(),
            table_id: "tbl".into(),
            app_id: "cli_test".into(),
            app_secret: "secret".into(),
            oauth_redirect: "http://127.0.0.1:8765/callback".into(),
        }
    }

    #[test]
    fn challenge_matches_the_rfc7636_worked_example() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge_of(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn state_and_verifier_differ_every_time() {
        let a = begin(&cfg(), "http://127.0.0.1:8765/callback");
        let b = begin(&cfg(), "http://127.0.0.1:8765/callback");
        assert_ne!(a.state, b.state);
        assert_ne!(a.verifier, b.verifier);
    }

    #[test]
    fn authorize_url_carries_pkce_and_scopes() {
        let p = begin(&cfg(), "http://127.0.0.1:8765/callback");
        assert!(p.url.contains("code_challenge_method=S256"));
        assert!(p.url.contains("code_challenge="));
        assert!(p.url.contains("response_type=code"));
        assert!(p.url.contains("offline_access"), "refresh needs this scope");
        assert!(!p.url.contains(&p.verifier), "the verifier must never be sent in the URL");
    }

    #[test]
    fn redirect_is_percent_encoded_in_the_url() {
        let p = begin(&cfg(), "http://127.0.0.1:8765/callback");
        assert!(p.url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8765%2Fcallback"));
    }

    #[test]
    fn loopback_redirects_are_accepted() {
        for r in [
            "http://127.0.0.1:8765/callback",
            "http://localhost:8765/callback",
        ] {
            assert!(validate_redirect(r).is_ok(), "{r} should be allowed");
        }
    }

    #[test]
    fn a_remote_redirect_is_refused() {
        // Would hand the authorization code to someone else.
        assert!(validate_redirect("http://evil.example.com:8765/callback").is_err());
        assert!(validate_redirect("https://evil.example.com/callback").is_err());
    }

    #[test]
    fn port_is_read_from_the_redirect() {
        assert_eq!(redirect_port("http://127.0.0.1:8765/callback").unwrap(), 8765);
        assert!(redirect_port("http://127.0.0.1/callback").is_err(), "port is required");
    }

    #[test]
    fn query_is_extracted_from_the_request_line() {
        let line = "GET /callback?code=abc&state=xyz HTTP/1.1";
        assert_eq!(query_of(line).unwrap(), "code=abc&state=xyz");
        assert!(query_of("GET /callback HTTP/1.1").is_none());
    }

    #[test]
    fn pairs_are_percent_decoded() {
        let got = parse_pairs("code=a%2Fb&state=x%20y");
        assert_eq!(got[0], ("code".into(), "a/b".into()));
        assert_eq!(got[1], ("state".into(), "x y".into()));
    }
}
