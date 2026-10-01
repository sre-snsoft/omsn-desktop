//! Startup configuration, validated at the boundary.
//!
//! Credentials are never compiled in. They come from `~/.config/omsn/`, the
//! same files the Phase 0 probe uses, so the app and the probe cannot drift.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::Result;

pub const ENV_FILE: &str = "desktop.env";
pub const TOKEN_FILE: &str = "token.json";

/// Built-in defaults, so a teammate can run the app with no setup at all.
///
/// None of these are secrets. The app id is already public — it travels in the
/// authorize URL — and the Base/table ids are addresses, not credentials:
/// reaching the data still requires that person's own Lark session and their
/// own Base permissions. Verified against the live tenant that Lark accepts a
/// PKCE exchange with no client secret, which is what makes this possible.
pub mod defaults {
    pub const APP_ID: &str = "cli_aa31efc9e838df0e";
    pub const BASE_TOKEN: &str = "SW1zbLwdNaYsyAsIpBElmk8rglh";
    pub const TABLE_ID: &str = "tblvOg8Ge09bG85d";
    pub const OAUTH_REDIRECT: &str = "http://localhost:8765/callback";
}

pub fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config").join("omsn")
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    pub base_token: String,
    pub table_id: String,
    pub app_id: String,
    /// Loopback URI registered in the Lark console for the sign-in redirect.
    pub oauth_redirect: String,
}

/// Parse `KEY=VALUE` lines, tolerating comments, blanks, `export ` and quotes.
pub fn parse_env(contents: &str) -> HashMap<String, String> {
    contents
        .lines()
        .filter_map(|raw| {
            let line = raw.trim().strip_prefix("export ").unwrap_or(raw.trim());
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            let unquoted = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            Some((key.trim().to_string(), unquoted.to_string()))
        })
        .collect()
}

impl AppConfig {
    /// Built-in defaults, optionally overridden by `~/.config/omsn/desktop.env`.
    ///
    /// A missing file is normal, not an error: the app must work on a fresh
    /// machine with nothing configured.
    pub fn load() -> Result<Self> {
        let path = config_dir().join(ENV_FILE);
        let values = std::fs::read_to_string(&path).map(|c| parse_env(&c)).unwrap_or_default();
        let pick = |key: &str, fallback: &str| -> String {
            std::env::var(key)
                .ok()
                .or_else(|| values.get(key).cloned())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| fallback.to_string())
        };
        Ok(AppConfig {
            base_token: pick("OMSN_BASE_TOKEN", defaults::BASE_TOKEN),
            table_id: pick("OMSN_TABLE_ID", defaults::TABLE_ID),
            app_id: pick("OMSN_LARK_APP_ID", defaults::APP_ID),
            oauth_redirect: pick("OMSN_OAUTH_REDIRECT", defaults::OAUTH_REDIRECT),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_pairs() {
        let env = parse_env("A=1\nB=two\n");
        assert_eq!(env.get("A").unwrap(), "1");
        assert_eq!(env.get("B").unwrap(), "two");
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let env = parse_env("# note\n\n  \nA=1\n");
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn strips_quotes_so_secrets_do_not_carry_them() {
        // A quoted secret would otherwise fail auth for a baffling reason.
        assert_eq!(parse_env("S=\"abc\"").get("S").unwrap(), "abc");
        assert_eq!(parse_env("S='abc'").get("S").unwrap(), "abc");
    }

    #[test]
    fn tolerates_an_export_prefix() {
        assert_eq!(parse_env("export A=1").get("A").unwrap(), "1");
    }

    #[test]
    fn keeps_equals_signs_inside_a_value() {
        assert_eq!(parse_env("A=a=b=c").get("A").unwrap(), "a=b=c");
    }
}

#[cfg(test)]
mod secret_hygiene {
    use super::*;

    fn cfg() -> AppConfig {
        AppConfig {
            base_token: "bascnTEST".into(),
            table_id: "tblTEST".into(),
            app_id: "cli_test".into(),
            oauth_redirect: "http://127.0.0.1:8765/callback".into(),
        }
    }

    /// `AppConfig` derives `Debug`, so the client secret is printed in full by
    /// any `{:?}`, `dbg!`, `unwrap()` panic or future log line that touches the
    /// config. Nothing does today, which is exactly why it will go unnoticed.
    #[test]
    fn debug_formatting_must_not_print_the_client_secret() {
        let rendered = format!("{:?}", cfg());
        assert!(
            !rendered.contains("SECRET-sentinel-9f3c1"),
            "AppConfig: Debug prints the client secret verbatim: {rendered}"
        );
    }

    /// A missing or blank key must name the key, never the value of any other.
    #[test]
    fn a_missing_key_error_names_only_the_key() {
        let env = parse_env("OMSN_LARK_APP_SECRET=SECRET-sentinel-9f3c1\n");
        assert!(env.get("OMSN_BASE_TOKEN").is_none());
        let err = crate::error::CoreError::Config(format!("{} is not set in {ENV_FILE}", "OMSN_BASE_TOKEN"));
        assert!(!err.to_string().contains("SECRET-sentinel-9f3c1"));
    }

    /// A blank value is as broken as a missing key; `need()` filters it, so
    /// pin that a blank secret is not silently carried into the token call.
    #[test]
    fn a_blank_value_is_treated_as_unset() {
        let env = parse_env("OMSN_LARK_APP_SECRET=\nOMSN_TABLE_ID=   \n");
        assert_eq!(env.get("OMSN_LARK_APP_SECRET").map(String::as_str), Some(""));
        assert_eq!(env.get("OMSN_TABLE_ID").map(String::as_str), Some(""));
    }

    /// Windows checkouts and hand-edited files carry CRLF; a trailing \r on a
    /// secret produces a 401 with no clue why.
    #[test]
    fn carriage_returns_do_not_survive_into_a_value() {
        let env = parse_env("OMSN_LARK_APP_SECRET=abc123\r\nOMSN_TABLE_ID=tbl\r\n");
        assert_eq!(
            env.get("OMSN_LARK_APP_SECRET").map(String::as_str),
            Some("abc123"),
            "a trailing CR would be sent as part of the secret"
        );
    }

    /// The redirect the whole sign-in depends on must be read verbatim.
    #[test]
    fn the_oauth_redirect_round_trips_unchanged() {
        let env = parse_env("OMSN_OAUTH_REDIRECT=\"http://127.0.0.1:8765/callback\"\n");
        assert_eq!(
            env.get("OMSN_OAUTH_REDIRECT").map(String::as_str),
            Some("http://127.0.0.1:8765/callback")
        );
    }
}

#[cfg(test)]
mod zero_config {
    use super::*;

    /// A teammate on a fresh machine has no env file and no env vars. The app
    /// must still know which Lark app and which Base to talk to, or a .dmg is
    /// useless without hand-placed config.
    #[test]
    fn load_succeeds_with_no_configuration_at_all() {
        // Guard against a developer's own exports leaking into the assertion.
        for key in ["OMSN_BASE_TOKEN", "OMSN_TABLE_ID", "OMSN_LARK_APP_ID", "OMSN_OAUTH_REDIRECT"] {
            if std::env::var(key).is_ok() {
                eprintln!("skipping: {key} is set in this environment");
                return;
            }
        }
        let cfg = AppConfig::load().expect("must not require a config file");
        assert_eq!(cfg.app_id, defaults::APP_ID);
        assert_eq!(cfg.base_token, defaults::BASE_TOKEN);
        assert_eq!(cfg.table_id, defaults::TABLE_ID);
        assert!(cfg.oauth_redirect.contains("8765"));
    }

    #[test]
    fn the_built_in_defaults_are_all_populated() {
        for (name, value) in [
            ("APP_ID", defaults::APP_ID),
            ("BASE_TOKEN", defaults::BASE_TOKEN),
            ("TABLE_ID", defaults::TABLE_ID),
            ("OAUTH_REDIRECT", defaults::OAUTH_REDIRECT),
        ] {
            assert!(!value.is_empty(), "{name} default must not be empty");
        }
    }
}
