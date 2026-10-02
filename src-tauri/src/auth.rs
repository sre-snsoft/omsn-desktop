//! Token lifecycle.
//!
//! Lark user access tokens last about two hours. Without refresh the app
//! simply stops working mid-session, and re-reading the cache cannot help
//! because the cached token is the expired one.
//!
//! Refresh is single-flighted on purpose: the poll loop and a user write can
//! both hit 401 at the same moment, and Lark **rotates the refresh token** on
//! every exchange. Two concurrent refreshes would leave the loser holding a
//! rotated-away token and log the user out at random.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{CoreError, Result};

const TOKEN_URL: &str = "https://open.larksuite.com/open-apis/authen/v2/oauth/token";
/// Refresh this long before actual expiry, so an in-flight request does not
/// race the deadline.
const EXPIRY_MARGIN_SECS: i64 = 300;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// Absolute unix seconds. Stored so a restart knows what it holds without
    /// a round trip.
    #[serde(default)]
    pub expires_at: i64,
}

impl TokenSet {
    pub fn is_usable(&self, now_secs: i64) -> bool {
        !self.access_token.is_empty() && now_secs < self.expires_at - EXPIRY_MARGIN_SECS
    }

    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.is_empty()
    }

    /// Build from Lark's token response, which reports a relative lifetime.
    pub fn from_response(body: &Value, now_secs: i64) -> Result<Self> {
        let pick = |key: &str| -> String {
            body.get(key)
                .and_then(Value::as_str)
                .or_else(|| body.get("data").and_then(|d| d.get(key)).and_then(Value::as_str))
                .unwrap_or_default()
                .to_string()
        };
        let access_token = pick("access_token");
        if access_token.is_empty() {
            // Never echo the body: it can carry a refresh token.
            return Err(CoreError::Auth("Lark returned no access token".into()));
        }
        let expires_in = body
            .get("expires_in")
            .and_then(Value::as_i64)
            .or_else(|| body.get("data").and_then(|d| d.get("expires_in")).and_then(Value::as_i64))
            .unwrap_or(7200);
        Ok(TokenSet {
            access_token,
            refresh_token: pick("refresh_token"),
            expires_at: now_secs + expires_in,
        })
    }
}

pub fn token_path() -> PathBuf {
    crate::config::config_dir().join(crate::config::TOKEN_FILE)
}

pub fn load_tokens() -> Result<TokenSet> {
    let path = token_path();
    let raw = std::fs::read_to_string(&path).map_err(|_| {
        CoreError::Auth("No saved session. Sign in to continue.".into())
    })?;
    serde_json::from_str(&raw)
        .map_err(|e| CoreError::Auth(format!("Saved session is unreadable: {e}")))
}

/// Persist tokens with owner-only permissions, replacing atomically.
pub fn save_tokens(tokens: &TokenSet) -> Result<()> {
    let path = token_path();
    let dir = path.parent().ok_or_else(|| CoreError::Config("bad token path".into()))?;
    std::fs::create_dir_all(dir).map_err(|e| CoreError::Keyring(e.to_string()))?;

    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string(tokens)
        .map_err(|e| CoreError::Keyring(format!("cannot serialise tokens: {e}")))?;
    std::fs::write(&tmp, body).map_err(|e| CoreError::Keyring(e.to_string()))?;
    set_owner_only(&tmp)?;
    std::fs::rename(&tmp, &path).map_err(|e| CoreError::Keyring(e.to_string()))?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| CoreError::Keyring(format!("cannot restrict {}: {e}", path.display())))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

/// Exchange a refresh token for a new pair. The refresh token rotates, so the
/// caller must persist the result.
/// Lark requires client authentication here; a secret-less refresh is refused
/// with invalid_client. Verified against the live tenant.
pub async fn refresh(
    http: &reqwest::Client,
    app_id: &str,
    app_secret: &str,
    refresh_token: &str,
    now_secs: i64,
) -> Result<TokenSet> {
    if refresh_token.is_empty() {
        return Err(CoreError::Auth("No refresh token; sign in again.".into()));
    }
    let resp = http
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": app_id,
            "client_secret": app_secret,
            "refresh_token": refresh_token,
        }))
        .send()
        .await?;

    let body: Value = resp.json().await?;
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(0);
    let oauth_error = body.get("error").and_then(Value::as_str);
    if code != 0 || oauth_error.is_some() {
        eprintln!(
            "OMSN refresh: rejected (code {:?}, error {:?}, {:?})",
            code,
            oauth_error,
            body.get("error_description").and_then(Value::as_str)
        );
        // A dead refresh token is the one case the user must act on.
        return Err(CoreError::Auth(
            "Your saved session is no longer valid. Sign in again.".into(),
        ));
    }
    TokenSet::from_response(&body, now_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_fresh_token_is_usable() {
        let t = TokenSet { access_token: "a".into(), refresh_token: "r".into(), expires_at: 10_000 };
        assert!(t.is_usable(1_000));
    }

    #[test]
    fn a_token_inside_the_margin_is_treated_as_expired() {
        // Avoids a request racing the deadline and failing mid-flight.
        let t = TokenSet { access_token: "a".into(), refresh_token: "r".into(), expires_at: 10_000 };
        assert!(!t.is_usable(10_000 - EXPIRY_MARGIN_SECS + 1));
    }

    #[test]
    fn an_empty_token_is_never_usable() {
        let t = TokenSet { access_token: String::new(), refresh_token: "r".into(), expires_at: i64::MAX };
        assert!(!t.is_usable(0));
    }

    #[test]
    fn response_parsing_converts_relative_lifetime_to_absolute() {
        let body = json!({"access_token": "abc", "refresh_token": "def", "expires_in": 7200});
        let t = TokenSet::from_response(&body, 1_000).unwrap();
        assert_eq!(t.access_token, "abc");
        assert_eq!(t.refresh_token, "def");
        assert_eq!(t.expires_at, 8_200);
    }

    #[test]
    fn response_parsing_reads_a_nested_data_object() {
        let body = json!({"code": 0, "data": {"access_token": "abc", "expires_in": 60}});
        assert_eq!(TokenSet::from_response(&body, 0).unwrap().expires_at, 60);
    }

    #[test]
    fn a_response_without_a_token_is_an_error_that_leaks_nothing() {
        let body = json!({"refresh_token": "SECRET-VALUE"});
        let err = TokenSet::from_response(&body, 0).unwrap_err().to_string();
        assert!(!err.contains("SECRET-VALUE"), "error must not echo the body");
    }

    // ---- Regression tests for the "Sign in does nothing" report ----

    /// The Phase 0 probe writes Lark's RAW token response to the same cache
    /// file. That body reports a *relative* lifetime (`expires_in`), so the
    /// absolute `expires_at` the Rust side needs is simply absent.
    #[test]
    fn a_probe_written_cache_deserialises_with_no_expiry_at_all() {
        let probe_body = r#"{
            "code": 0,
            "msg": "success",
            "access_token": "u-brand-new-token",
            "refresh_token": "ur-brand-new-refresh",
            "expires_in": 7200,
            "refresh_token_expires_in": 2592000,
            "token_type": "Bearer",
            "scope": "bitable:app contact:user.base:readonly offline_access"
        }"#;
        let t: TokenSet = serde_json::from_str(probe_body).unwrap();
        assert_eq!(t.access_token, "u-brand-new-token");
        // The lifetime is silently dropped on the floor.
        assert_eq!(t.expires_at, 0, "expires_in is not read; expires_at defaults to 0");
    }

    /// Consequence of the above: a token minted one second ago is classed as
    /// long expired, so every start forces a refresh round trip.
    #[test]
    fn a_probe_written_cache_is_never_usable_however_fresh() {
        let t: TokenSet = serde_json::from_str(
            r#"{"access_token": "fresh", "refresh_token": "r", "expires_in": 7200}"#,
        )
        .unwrap();
        let now = 1_790_000_000;
        assert!(!t.is_usable(now), "a valid token is treated as expired");
        assert!(t.can_refresh(), "recovery depends entirely on the refresh token");
    }

    /// The same cache with no `offline_access` grant has nothing to fall back
    /// on: not usable and not refreshable. This is the dead end.
    #[test]
    fn a_probe_cache_without_a_refresh_token_is_a_dead_end() {
        let t: TokenSet = serde_json::from_str(
            r#"{"access_token": "fresh", "expires_in": 7200}"#,
        )
        .unwrap();
        assert!(!t.is_usable(1_790_000_000));
        assert!(!t.can_refresh(), "no way back: access_token() can only return Unauthorized");
    }

    /// Some Lark responses nest the payload under `data`. `access_token` has
    /// no serde default, so that shape does not deserialise at all.
    #[test]
    fn a_cache_nested_under_data_fails_to_load_entirely() {
        let nested = r#"{"code": 0, "data": {"access_token": "a", "expires_in": 7200}}"#;
        let parsed: std::result::Result<TokenSet, _> = serde_json::from_str(nested);
        assert!(parsed.is_err(), "nested caches are unreadable, not merely expired");
    }

    /// `from_response` does the conversion correctly — proving the bug is the
    /// cache *format*, not the parser, and that a successful refresh heals it.
    #[test]
    fn a_successful_refresh_rewrites_the_cache_into_the_absolute_form() {
        let body = serde_json::from_str::<Value>(
            r#"{"code": 0, "access_token": "a", "refresh_token": "r", "expires_in": 7200}"#,
        )
        .unwrap();
        let healed = TokenSet::from_response(&body, 1_790_000_000).unwrap();
        assert_eq!(healed.expires_at, 1_790_007_200);
        assert!(healed.is_usable(1_790_000_000));
        // And save_tokens serialises only the three fields the Rust side reads.
        let written = serde_json::to_string(&healed).unwrap();
        assert!(written.contains("expires_at"));
        assert!(!written.contains("expires_in"), "the relative form must not survive a save");
    }

    /// A refresh attempt with nothing to refresh must not hit the network.
    #[tokio::test]
    async fn refresh_with_an_empty_token_fails_locally() {
        let http = reqwest::Client::new();
        let err = refresh(&http, "id", "secret", "", 0).await.unwrap_err();
        assert!(matches!(err, CoreError::Auth(_)));
    }
}
