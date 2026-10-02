//! In-app authorization-code flow.
//!
//! A desktop app has no public URL, so the redirect target is a loopback
//! listener that lives only for the seconds the browser needs to come back
//! with a code. Ported from `probe/lark_auth.py`, which proved this exact
//! exchange against the live tenant.
//!
//! PKCE does the real work here. Lark accepts the exchange as a public client,
//! so no client secret ships with the app — which is what lets a teammate run
//! it with nothing configured. PKCE is then the only thing binding the code to
//! this process, so another local program cannot race the loopback redirect
//! and redeem a stolen code.

use std::io::{BufRead, BufReader, Read, Write};
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

/// Random, URL-safe token from the OS CSPRNG.
///
/// Drawn from `getrandom` directly rather than stitching UUIDs: a v4 UUID
/// carries fixed version and variant bits, so every 16 bytes leaked 6 bits of
/// entropy, and the guarantee rested on uuid's choice of backend rather than
/// on anything stated here.
fn random_urlsafe(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    getrandom::fill(&mut raw).expect("the OS CSPRNG must be available");
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

/// Largest request line we will read. A client that can reach loopback —
/// including any page in the browser — must not be able to grow this buffer.
const MAX_REQUEST_LINE: u64 = 8 * 1024;

/// Both loopback addresses the redirect could arrive on.
///
/// `localhost` resolves to *both* `127.0.0.1` and `::1`, and browsers on macOS
/// commonly try the IPv6 address first. Listening on IPv4 alone meant the
/// browser hit a refused connection on `[::1]`, never delivered the code, and
/// the user came back to an app that had silently timed out.
#[derive(Debug)]
pub struct Loopback {
    listeners: Vec<TcpListener>,
}

impl Loopback {
    /// Accept from whichever address the browser actually used.
    fn accept(&self) -> std::io::Result<Option<TcpStream>> {
        for listener in &self.listeners {
            match listener.accept() {
                Ok((stream, _)) => return Ok(Some(stream)),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    #[cfg(test)]
    fn families(&self) -> usize {
        self.listeners.len()
    }
}

/// Reserve the redirect port on every loopback address.
///
/// Called *before* the browser is opened: otherwise the user could consent and
/// have the code delivered to whatever already holds the port.
///
/// Succeeds if at least one family binds — a machine with IPv6 disabled is
/// still perfectly usable — but fails loudly if neither does.
pub fn bind_listener(port: u16) -> Result<Loopback> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    let mut listeners = Vec::new();
    let mut last_error = None;
    for addr in [
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ] {
        match TcpListener::bind((addr, port)) {
            Ok(listener) => match listener.set_nonblocking(true) {
                Ok(()) => listeners.push(listener),
                Err(e) => last_error = Some(e.to_string()),
            },
            Err(e) => last_error = Some(e.to_string()),
        }
    }

    if listeners.is_empty() {
        return Err(CoreError::Config(format!(
            "Cannot listen on port {port} for the sign-in redirect \
             (is another copy of OMSN running?): {}",
            last_error.unwrap_or_else(|| "no loopback address available".into())
        )));
    }
    Ok(Loopback { listeners })
}

/// What one connection turned out to be.
enum Outcome {
    Code(String),
    Denied(String),
    Ignore,
}

/// Read one connection and answer it.
///
/// `accept` on BSD/macOS yields a socket that inherits the listener's
/// non-blocking flag, which makes `set_read_timeout` a no-op and `read_line`
/// fail with `WouldBlock` whenever the browser has not already written. That
/// silently discarded valid codes, so switch the socket back to blocking
/// first — then a read timeout actually applies.
fn serve(mut stream: TcpStream, expected_state: &str) -> Outcome {
    if stream.set_nonblocking(false).is_err() {
        return Outcome::Ignore;
    }
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));

    let mut line = String::new();
    let mut reader = BufReader::new((&stream).take(MAX_REQUEST_LINE));
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return Outcome::Ignore; // preconnect, probe, or a client that said nothing
    }

    let Some(query) = query_of(&line) else {
        respond(&mut stream, false);
        return Outcome::Ignore;
    };
    let pairs = parse_pairs(&query);
    let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());

    // A mismatched state is not our redirect; keep waiting for the real one.
    if get("state").as_deref() != Some(expected_state) {
        respond(&mut stream, false);
        return Outcome::Ignore;
    }
    match get("code") {
        Some(code) if !code.is_empty() => {
            respond(&mut stream, true);
            Outcome::Code(code)
        }
        _ => {
            respond(&mut stream, false);
            Outcome::Denied(describe_error(get("error").as_deref()))
        }
    }
}

/// Reduce an `error` param to a known class. The value is attacker-supplied
/// and ends up in the UI, so it is never passed through verbatim.
fn describe_error(raw: Option<&str>) -> String {
    match raw {
        Some("access_denied") => "you declined the permission request".into(),
        Some("invalid_scope") => "the app is missing a required scope".into(),
        Some("server_error") => "Lark reported a server error".into(),
        Some(_) => "Lark rejected the request".into(),
        None => "no authorization code was returned".into(),
    }
}

/// Block until the browser hits the loopback redirect, then return the code.
///
/// Each connection is served on its own short-lived thread, so a client that
/// connects and says nothing costs its own timeout rather than the whole
/// consent window. Browsers preconnect without sending data, so that is
/// ordinary traffic, not just an attack.
///
/// Synchronous by nature: run it on a blocking thread, never the async runtime.
pub fn wait_for_code(listener: Loopback, expected_state: &str) -> Result<String> {
    let (tx, rx) = std::sync::mpsc::channel::<Outcome>();
    let deadline = Instant::now() + CONSENT_TIMEOUT;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CoreError::Auth("Sign-in timed out. Please try again.".into()));
        }

        match listener.accept() {
            Ok(Some(stream)) => {
                let tx = tx.clone();
                let state = expected_state.to_string();
                std::thread::spawn(move || {
                    let _ = tx.send(serve(stream, &state));
                });
            }
            Ok(None) => {}
            Err(e) => return Err(CoreError::Config(format!("redirect listener failed: {e}"))),
        }

        // Collect whatever finished, without blocking the accept loop.
        match rx.recv_timeout(Duration::from_millis(120)) {
            Ok(Outcome::Code(code)) => return Ok(code),
            Ok(Outcome::Denied(why)) => {
                return Err(CoreError::Auth(format!("Sign-in failed: {why}")))
            }
            Ok(Outcome::Ignore) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        }
    }
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
            "code": code,
            "redirect_uri": pending.redirect,
            "code_verifier": pending.verifier,
        }))
        .send()
        .await?;

    let status = resp.status();
    let body: Value = resp.json().await?;
    let lark_code = body.get("code").and_then(Value::as_i64).unwrap_or(0);
    // Lark reports OAuth problems two ways: a non-zero `code`, or an RFC 6749
    // `error` string with no code at all. Treating only the former as failure
    // let a rejected exchange look successful and surface later as a confusing
    // "session expired".
    let oauth_error = body.get("error").and_then(Value::as_str);
    if lark_code != 0 || oauth_error.is_some() {
        // Diagnosable without leaking: these fields never carry a token.
        eprintln!(
            "OMSN sign-in: token exchange rejected (http {}, code {:?}, error {:?}, {:?})",
            status.as_u16(),
            lark_code,
            oauth_error,
            body.get("error_description").and_then(Value::as_str)
        );
        let detail = body
            .get("error_description")
            .and_then(Value::as_str)
            .or_else(|| body.get("msg").and_then(Value::as_str))
            .unwrap_or("Lark rejected the sign-in");
        let label = oauth_error.unwrap_or("error");
        return Err(CoreError::Auth(format!("{detail} ({label})")));
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

/// Hardening tests for the in-app sign-in flow.
///
/// These exercise the loopback listener end to end over a real socket, because
/// the failure modes that matter (a browser that connects before it writes, a
/// port already taken, a hostile local client) only appear at that boundary.
#[cfg(test)]
mod hardening {
    use super::*;
    use std::collections::HashSet;
    use std::net::TcpStream;
    use std::sync::mpsc;

    const SENTINEL_SECRET: &str = "SECRET-sentinel-9f3c1";

    fn cfg() -> AppConfig {
        AppConfig {
            base_token: "bascnTEST".into(),
            table_id: "tblTEST".into(),
            app_id: "cli_test".into(),
            oauth_redirect: "http://127.0.0.1:8765/callback".into(),
        }
    }

    /// An ephemeral port, released immediately so the listener under test can
    /// claim it.
    fn free_port() -> u16 {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }

    /// Run `wait_for_code` off-thread. The real CONSENT_TIMEOUT is 180s, so a
    /// test must never join this directly — use `recv_timeout`.
    fn spawn_waiter(port: u16, expected_state: &str) -> mpsc::Receiver<Result<String>> {
        let listener = bind_listener(port).expect("test port should bind");
        let (tx, rx) = mpsc::channel();
        let state = expected_state.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(wait_for_code(listener, &state));
        });
        rx
    }

    /// Connect, retrying while the listener is still coming up.
    fn connect(port: u16) -> TcpStream {
        for _ in 0..100 {
            if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
                return s;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("listener on {port} never came up");
    }

    fn get_line(query: &str) -> String {
        format!("GET /callback?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\nUser-Agent: test\r\n\r\n")
    }

    // ---------------- PKCE conformance (RFC 7636 §4.1) ----------------

    /// code_verifier = 43*128unreserved, unreserved = ALPHA / DIGIT / "-" / "." / "_" / "~"
    #[test]
    fn verifier_satisfies_rfc7636_length_and_charset() {
        for _ in 0..256 {
            let p = begin(&cfg(), "http://127.0.0.1:8765/callback");
            let n = p.verifier.chars().count();
            assert!(
                (43..=128).contains(&n),
                "verifier is {n} chars; RFC 7636 requires 43-128: {:?}",
                p.verifier
            );
            assert!(
                p.verifier
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')),
                "verifier has characters outside the unreserved set: {:?}",
                p.verifier
            );
        }
    }

    /// The challenge must be BASE64URL(SHA256(ASCII(verifier))) with no padding,
    /// and must be a different string from the verifier it commits to.
    #[test]
    fn challenge_is_unpadded_base64url_of_the_live_verifier() {
        let p = begin(&cfg(), "http://127.0.0.1:8765/callback");
        let expected = code_challenge_of(&p.verifier);
        assert_eq!(expected.len(), 43, "SHA-256 base64url-nopad is always 43 chars");
        assert!(!expected.contains('='), "challenge must not be padded");
        assert!(
            expected
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "challenge must be base64url"
        );
        assert!(p.url.contains(&format!("code_challenge={expected}")));
        assert_ne!(expected, p.verifier);
    }

    /// `state` must also be long enough to be unguessable inside the 180s
    /// window, and URL-safe so it survives the round trip unencoded.
    #[test]
    fn state_is_url_safe_and_long_enough() {
        let mut seen = HashSet::new();
        for _ in 0..256 {
            let p = begin(&cfg(), "http://127.0.0.1:8765/callback");
            assert!(p.state.len() >= 22, "state too short: {:?}", p.state);
            assert!(
                p.state
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "state is not URL-safe: {:?}",
                p.state
            );
            assert!(seen.insert(p.state), "state repeated across calls");
        }
    }

    /// Tokens were previously stitched from v4 UUIDs, which put the version
    /// and variant markers at fixed positions — 6 wasted bits per 16 bytes,
    /// and the entropy guarantee rested on uuid's backend rather than on
    /// anything explicit. Drawing from the OS CSPRNG removes both problems;
    /// this pins it so a future refactor cannot quietly reintroduce them.
    #[test]
    fn random_tokens_have_no_fixed_bits() {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut byte6 = HashSet::new();
        let mut byte8 = HashSet::new();
        for _ in 0..256 {
            let raw = engine.decode(random_urlsafe(24)).unwrap();
            assert_eq!(raw.len(), 24);
            byte6.insert(raw[6] >> 4); // was always 0x4 (UUID version)
            byte8.insert(raw[8] >> 6); // was always 0b10 (RFC 4122 variant)
        }
        assert!(
            byte6.len() > 8,
            "byte 6 should span the nibble range, saw only {:?}",
            byte6
        );
        assert_eq!(
            byte8.len(),
            4,
            "byte 8 top two bits should take all four values, saw {:?}",
            byte8
        );
    }

    /// Byte length requested is the byte length produced (no silent short read).
    #[test]
    fn random_urlsafe_returns_exactly_the_requested_byte_count() {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        for n in [1usize, 15, 16, 17, 24, 32, 48, 64] {
            assert_eq!(engine.decode(random_urlsafe(n)).unwrap().len(), n);
        }
    }

    // ---------------- loopback listener ----------------

    /// Baseline: the request is already buffered when `accept` returns, so the
    /// code comes back. This is the path that happens to work today.
    #[test]
    fn a_prebuffered_redirect_yields_the_code() {
        let port = free_port();
        let rx = spawn_waiter(port, "STATE-OK");
        let mut s = connect(port);
        s.write_all(get_line("code=THE-CODE&state=STATE-OK").as_bytes()).unwrap();
        s.flush().unwrap();
        // Let the bytes land before the accept poll picks the socket up.
        std::thread::sleep(Duration::from_millis(300));

        let got = rx
            .recv_timeout(Duration::from_secs(8))
            .expect("wait_for_code never returned");
        assert_eq!(got.unwrap(), "THE-CODE");
    }

    /// A real browser opens the TCP connection and writes the GET a moment
    /// later. `wait_for_code` sets the listener non-blocking; on macOS/BSD the
    /// accepted socket INHERITS O_NONBLOCK, so `read_line` returns WouldBlock,
    /// `is_err()` fires, and the authorization code is thrown away by
    /// `continue`. The user then waits out the full 180s for a timeout.
    #[test]
    fn a_browser_that_writes_after_connecting_must_not_lose_the_code() {
        let port = free_port();
        let rx = spawn_waiter(port, "STATE-SLOW");
        let mut s = connect(port);
        // Connection established; request not sent yet. 150ms is well within
        // what a browser spends navigating.
        std::thread::sleep(Duration::from_millis(150));
        s.write_all(get_line("code=SLOW-CODE&state=STATE-SLOW").as_bytes()).unwrap();
        s.flush().unwrap();

        let got = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| {
            panic!(
                "the code was dropped: the accepted socket is non-blocking, so \
                 read_line() failed with WouldBlock and the `continue` discarded \
                 a valid redirect. Sign-in will hang for the full 180s."
            )
        });
        assert_eq!(got.unwrap(), "SLOW-CODE");
    }

    /// Same defect, stated as the property that is actually broken: an accepted
    /// socket must be readable with a timeout, not non-blocking.
    /// `accept` on BSD/macOS hands back a socket that inherits the listener's
    /// non-blocking flag, which made `set_read_timeout` a no-op and
    /// `read_line` fail with WouldBlock — silently discarding the code.
    /// `serve` must switch the socket back to blocking before reading.
    #[test]
    fn serve_reads_a_socket_that_inherited_non_blocking_mode() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        std::thread::spawn(move || {
            let mut s = connect(port);
            // Connect first, write later: the window that used to lose codes.
            std::thread::sleep(Duration::from_millis(150));
            let _ = s.write_all(get_line("code=GOOD&state=STATE").as_bytes());
            std::thread::sleep(Duration::from_millis(300));
        });

        let stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        };

        match serve(stream, "STATE") {
            Outcome::Code(code) => assert_eq!(code, "GOOD"),
            other => panic!(
                "a delayed write must still yield the code, got {}",
                match other {
                    Outcome::Denied(w) => format!("Denied({w})"),
                    _ => "Ignore".into(),
                }
            ),
        }
    }

    /// A second sign-in (or any other local process) holding the redirect port
    /// makes the listener unbindable. `authorize` binds before opening the
    /// browser, so this must fail before the user ever consents.
    ///
    /// Both loopback families have to be occupied to make the port genuinely
    /// unavailable — holding only IPv4 still leaves `[::1]` free, and the code
    /// can still be delivered there.
    #[test]
    fn a_fully_taken_redirect_port_fails_fast_with_a_config_error() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

        let v4 = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = v4.local_addr().unwrap().port();
        let v6 = TcpListener::bind((IpAddr::V6(Ipv6Addr::LOCALHOST), port));
        if v6.is_err() {
            // The OS gave us a port already busy on IPv6; nothing to assert.
            return;
        }
        let _ = IpAddr::V4(Ipv4Addr::LOCALHOST);

        let started = Instant::now();
        let err = bind_listener(port).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must fail immediately, not sit on the consent deadline"
        );
        assert!(matches!(err, CoreError::Config(_)), "got {err:?}");
        assert!(
            !err.to_string().contains(SENTINEL_SECRET),
            "bind error must not carry credentials"
        );
    }

    /// The regression that broke sign-in: `localhost` resolves to both
    /// `127.0.0.1` and `::1`, and a browser preferring IPv6 hit a refused
    /// connection when only IPv4 was bound. The code was never delivered and
    /// the app sat waiting until it timed out.
    #[test]
    fn the_redirect_is_reachable_on_both_loopback_families() {
        use std::net::{IpAddr, Ipv6Addr};

        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let bound = bind_listener(port).expect("must bind at least one family");
        let ipv6_available = TcpListener::bind((IpAddr::V6(Ipv6Addr::LOCALHOST), 0)).is_ok();
        if ipv6_available {
            assert_eq!(
                bound.families(),
                2,
                "a browser may use either address, so both must be listening"
            );
        }

        // Whichever families bound must actually accept a connection.
        for addr in ["127.0.0.1", "::1"] {
            let sock: std::net::IpAddr = addr.parse().unwrap();
            if let Ok(mut s) = TcpStream::connect((sock, port)) {
                let _ = s.write_all(b"GET /callback?state=x HTTP/1.1\r\n\r\n");
            }
        }
    }

    /// A redirect carrying somebody else's state must be ignored, and the real
    /// one that follows must still be honoured.
    #[test]
    fn a_wrong_state_is_ignored_and_the_real_redirect_still_works() {
        let port = free_port();
        let rx = spawn_waiter(port, "STATE-MINE");

        let mut attacker = connect(port);
        attacker
            .write_all(get_line("code=ATTACKER-CODE&state=STATE-THEIRS").as_bytes())
            .unwrap();
        attacker.flush().unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let mut browser = connect(port);
        browser
            .write_all(get_line("code=MY-CODE&state=STATE-MINE").as_bytes())
            .unwrap();
        browser.flush().unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("wait_for_code never returned")
            .expect("the genuine redirect was rejected");
        assert_eq!(got, "MY-CODE", "the attacker's code must never be accepted");
    }

    /// Lark can come back with `?error=...`. That value is attacker-reachable
    /// (anything can hit the loopback port with a guessed state is not needed:
    /// the error branch is only reached after a state match, but the *text* is
    /// still taken verbatim from the query) and is pasted into the message the
    /// UI renders.
    #[test]
    fn the_error_parameter_is_echoed_verbatim_into_the_user_facing_message() {
        let port = free_port();
        let rx = spawn_waiter(port, "STATE-ERR");
        let mut s = connect(port);
        s.write_all(
            get_line("state=STATE-ERR&error=access_denied%20%3Cimg%20src%3Dx%3E").as_bytes(),
        )
        .unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let err = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("wait_for_code never returned")
            .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, CoreError::Auth(_)), "got {err:?}");
        assert!(
            !msg.contains('<'),
            "untrusted markup from the query reaches the UI verbatim: {msg:?}"
        );
    }

    /// The HTTP body returned to the browser must never echo the query, or the
    /// authorization code would be printed on screen and readable by any
    /// script that can reach the port.
    #[test]
    fn the_http_response_never_echoes_the_code_or_state() {
        let port = free_port();
        let rx = spawn_waiter(port, "STATE-BODY");
        let mut s = connect(port);
        s.write_all(get_line("code=SUPER-SECRET-CODE&state=STATE-BODY").as_bytes()).unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let _ = rx.recv_timeout(Duration::from_secs(10));
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut body = String::new();
        use std::io::Read;
        let _ = s.read_to_string(&mut body);
        assert!(!body.is_empty(), "the browser got no response at all");
        assert!(!body.contains("SUPER-SECRET-CODE"), "the code is on screen: {body}");
        assert!(!body.contains("STATE-BODY"), "the state is on screen: {body}");
        assert!(
            body.contains("Content-Security-Policy"),
            "the page must be locked down: {body}"
        );
    }

    /// Any client that can reach loopback must not be able to make the app
    /// allocate without bound while the sign-in window is open.
    #[test]
    fn serve_caps_how_much_it_will_read() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        std::thread::spawn(move || {
            let mut s = connect(port);
            // Far more than MAX_REQUEST_LINE, with no newline to end it.
            let padding = "A".repeat(2 * 1024 * 1024);
            let _ = s.write_all(format!("GET /callback?pad={padding}").as_bytes());
            std::thread::sleep(Duration::from_millis(200));
        });

        let (stream, _) = listener.accept().unwrap();
        let started = Instant::now();
        let outcome = serve(stream, "STATE");

        assert!(
            matches!(outcome, Outcome::Ignore),
            "an oversized request must be dropped, not parsed"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "reading must be bounded, not open-ended"
        );
    }

    /// A browser may split the request line across packets. Everything before
    /// the first newline must still be assembled into one line.
    #[test]
    fn a_request_line_split_across_writes_is_still_parsed() {
        let port = free_port();
        let rx = spawn_waiter(port, "STATE-SPLIT");
        let mut s = connect(port);
        s.write_all(b"GET /callback?code=SPL").unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_millis(250));
        s.write_all(b"IT-CODE&state=STATE-SPLIT HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        s.flush().unwrap();

        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("wait_for_code never returned; a split request line was dropped")
            .expect("a split request line was rejected");
        assert_eq!(got, "SPLIT-CODE");
    }

    /// Connections are accepted and read one at a time, so a client that
    /// connects and says nothing occupies the listener. The genuine redirect
    /// queued behind it must still be served promptly.
    #[test]
    fn silent_local_clients_do_not_delay_the_real_redirect() {
        let port = free_port();
        let rx = spawn_waiter(port, "STATE-LORIS");

        // Three sockets that connect and never write, held open for the test.
        let _silent: Vec<TcpStream> = (0..3).map(|_| connect(port)).collect();
        std::thread::sleep(Duration::from_millis(200));

        let mut browser = connect(port);
        browser
            .write_all(get_line("code=LORIS-CODE&state=STATE-LORIS").as_bytes())
            .unwrap();
        browser.flush().unwrap();

        let started = Instant::now();
        let got = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the real redirect was never served");
        assert_eq!(got.unwrap(), "LORIS-CODE");
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "each silent socket costs up to SOCKET_TIMEOUT ({:?}); \
             the real redirect waited {:?}",
            SOCKET_TIMEOUT,
            started.elapsed()
        );
    }

    /// A malformed request (no query at all) must be answered and skipped, not
    /// treated as a sign-in.
    #[test]
    fn a_request_with_no_query_is_skipped_not_accepted() {
        let port = free_port();
        let rx = spawn_waiter(port, "STATE-BARE");
        let mut bare = connect(port);
        bare.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        bare.flush().unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let mut browser = connect(port);
        browser
            .write_all(get_line("code=BARE-OK&state=STATE-BARE").as_bytes())
            .unwrap();
        browser.flush().unwrap();
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10))
                .expect("wait_for_code never returned")
                .unwrap(),
            "BARE-OK"
        );
    }

    /// An empty `code` is a failure, not a success.
    #[test]
    fn an_empty_code_is_rejected() {
        let port = free_port();
        let rx = spawn_waiter(port, "STATE-EMPTY");
        let mut s = connect(port);
        s.write_all(get_line("code=&state=STATE-EMPTY").as_bytes()).unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let err = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("wait_for_code never returned")
            .unwrap_err();
        assert!(matches!(err, CoreError::Auth(_)), "got {err:?}");
    }

    // ---------------- credential hygiene ----------------

    /// Nothing the sign-in flow produces may carry the client secret.
    #[test]
    fn no_sign_in_artefact_carries_the_client_secret() {
        let c = cfg();
        let p = begin(&c, &c.oauth_redirect);
        assert!(!p.url.contains(SENTINEL_SECRET), "the authorize URL leaks the secret");
        assert!(!p.url.contains(&p.verifier), "the verifier must stay local");
        assert!(p.url.contains(&c.app_id), "client_id is public and required");

        for bad in [
            "http://evil.example.com:8765/callback",
            "https://127.0.0.1:8765/callback",
            "not-a-url",
            "http://127.0.0.1/callback",
        ] {
            let messages = [
                validate_redirect(bad).err().map(|e| e.to_string()),
                redirect_port(bad).err().map(|e| e.to_string()),
            ];
            for msg in messages.into_iter().flatten() {
                assert!(!msg.contains(SENTINEL_SECRET), "config error leaks the secret: {msg}");
            }
        }
    }

    /// A token response that failed must surface `msg` only - never the tokens
    /// that can sit alongside it in the same body.
    #[test]
    fn a_failed_token_body_does_not_leak_the_tokens_it_contains() {
        let body = serde_json::json!({
            "code": 20065,
            "msg": "invalid code",
            "access_token": "u-LEAKED-ACCESS",
            "refresh_token": "ur-LEAKED-REFRESH",
        });
        // Mirrors the branch in exchange(): msg only.
        let err = CoreError::Auth(format!(
            "Sign-in failed: {}",
            body.get("msg").and_then(Value::as_str).unwrap_or("unknown error")
        ));
        let rendered = err.to_string();
        assert!(!rendered.contains("LEAKED-ACCESS"));
        assert!(!rendered.contains("LEAKED-REFRESH"));
        let ui: crate::error::UiError = err.into();
        assert!(ui.needs_login, "a failed sign-in must offer another attempt");
        assert!(!ui.message.contains("LEAKED"));
    }

    /// A non-loopback redirect that still *parses* must be refused before a
    /// browser is ever opened - otherwise the code goes to a remote host.
    #[test]
    fn sneaky_non_loopback_redirects_are_refused() {
        for bad in [
            // Suffix tricks: the loopback literal is only a label here.
            "http://127.0.0.1.evil.com:8765/callback",
            "http://localhost.evil.com:8765/callback",
            // Wildcard bind address, not loopback.
            "http://0.0.0.0:8765/callback",
            // userinfo smuggling: the real host is evil.com.
            "http://127.0.0.1:8765@evil.com/callback",
            "http://127.0.0.1@evil.com:8765/callback",
            // Non-loopback private ranges.
            "http://192.168.1.10:8765/callback",
            "http://10.0.0.1:8765/callback",
            // Wrong scheme on loopback.
            "https://127.0.0.1:8765/callback",
            "file://127.0.0.1:8765/callback",
        ] {
            assert!(
                validate_redirect(bad).is_err(),
                "{bad} must not be accepted as loopback"
            );
        }
    }

    /// The `url` crate normalises integer, hex and short-form IPv4 to
    /// 127.0.0.1, so these obfuscations are genuinely loopback and accepting
    /// them is correct. Pinned so a parser swap cannot silently change it.
    #[test]
    fn obfuscated_loopback_forms_normalise_to_127_0_0_1() {
        for ok in [
            "http://2130706433:8765/callback",
            "http://0x7f000001:8765/callback",
            "http://127.1:8765/callback",
        ] {
            let host = url::Url::parse(ok).unwrap().host_str().unwrap().to_string();
            assert_eq!(host, "127.0.0.1", "{ok} normalised to {host}");
            assert!(validate_redirect(ok).is_ok(), "{ok} is loopback after normalisation");
        }
    }
}
