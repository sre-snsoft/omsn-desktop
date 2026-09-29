//! Startup configuration, validated at the boundary.
//!
//! Credentials are never compiled in. They come from `~/.config/omsn/`, the
//! same files the Phase 0 probe uses, so the app and the probe cannot drift.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::{CoreError, Result};

pub const ENV_FILE: &str = "desktop.env";
pub const TOKEN_FILE: &str = "token.json";

pub fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config").join("omsn")
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    pub base_token: String,
    pub table_id: String,
    /// Needed to refresh an expired session without a browser round trip.
    pub app_id: String,
    pub app_secret: String,
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
    pub fn load() -> Result<Self> {
        let path = config_dir().join(ENV_FILE);
        let contents = std::fs::read_to_string(&path).map_err(|e| {
            CoreError::Config(format!("Cannot read {}: {e}", path.display()))
        })?;
        let values = parse_env(&contents);

        let need = |key: &str| -> Result<String> {
            values
                .get(key)
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or_else(|| CoreError::Config(format!("{key} is not set in {ENV_FILE}")))
        };

        Ok(AppConfig {
            base_token: need("OMSN_BASE_TOKEN")?,
            table_id: need("OMSN_TABLE_ID")?,
            app_id: need("OMSN_LARK_APP_ID")?,
            app_secret: need("OMSN_LARK_APP_SECRET")?,
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
