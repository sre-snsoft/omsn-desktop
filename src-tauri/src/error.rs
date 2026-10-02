//! Error types shared by the whole core.
//!
//! Every variant carries a message that is safe to show a user: no tokens, no
//! secrets, no raw payloads. Detail for engineers goes to the log, not here.

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("Configuration problem: {0}")]
    Config(String),

    /// A value the user supplied that the Base would not accept. Distinct
    /// from `Config`: nothing is wrong with the installation, so prefixing it
    /// with "Configuration problem" would send the reader to the wrong file.
    #[error("{0}")]
    Invalid(String),

    #[error("Could not reach Lark. Check your connection and try again.")]
    Network(#[from] reqwest::Error),

    #[error("Your session expired. Please sign in again.")]
    Unauthorized,

    #[error("You do not have permission to do that in this Base.")]
    Forbidden,

    #[error("Lark is rate limiting us. Wait a moment and retry.")]
    RateLimited,

    #[error("Lark returned an error ({code}): {message}")]
    Api { code: i64, message: String },

    #[error("Sign-in failed: {0}")]
    Auth(String),

    #[error("Could not store credentials securely: {0}")]
    Keyring(String),
}

impl CoreError {
    /// Map a Lark business code onto a typed error.
    ///
    /// Lark returns HTTP 200 with a non-zero `code` for most failures, so the
    /// HTTP status alone is never sufficient to decide success.
    pub fn from_lark_code(code: i64, message: impl Into<String>) -> Self {
        match code {
            99991663 | 99991661 | 20005 => CoreError::Unauthorized,
            1254302 | 1254043 | 91403 => CoreError::Forbidden,
            99991400 => CoreError::RateLimited,
            _ => CoreError::Api { code, message: message.into() },
        }
    }
}

/// Serialised form sent across the Tauri bridge to the UI.
#[derive(Serialize)]
pub struct UiError {
    pub kind: String,
    pub message: String,
    /// True when signing in again is likely to fix it.
    pub needs_login: bool,
}

impl From<CoreError> for UiError {
    fn from(err: CoreError) -> Self {
        let kind = match &err {
            CoreError::Config(_) => "config",
            CoreError::Invalid(_) => "invalid",
            CoreError::Network(_) => "network",
            CoreError::Unauthorized => "unauthorized",
            CoreError::Forbidden => "forbidden",
            CoreError::RateLimited => "rate_limited",
            CoreError::Api { .. } => "api",
            CoreError::Auth(_) => "auth",
            CoreError::Keyring(_) => "keyring",
        };
        UiError {
            kind: kind.to_string(),
            message: err.to_string(),
            needs_login: matches!(err, CoreError::Unauthorized | CoreError::Auth(_)),
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_codes_map_to_unauthorized() {
        assert!(matches!(CoreError::from_lark_code(99991663, "x"), CoreError::Unauthorized));
    }

    #[test]
    fn permission_codes_map_to_forbidden() {
        assert!(matches!(CoreError::from_lark_code(1254302, "x"), CoreError::Forbidden));
    }

    #[test]
    fn unknown_code_is_preserved_for_diagnosis() {
        match CoreError::from_lark_code(12345, "boom") {
            CoreError::Api { code, message } => {
                assert_eq!(code, 12345);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn an_invalid_value_does_not_read_as_a_broken_installation() {
        // "Configuration problem: A task needs a title." sends the user to
        // ~/.config/omsn/desktop.env for a typo they made in a text box.
        let ui: UiError = CoreError::Invalid("A task needs a title.".into()).into();
        assert_eq!(ui.message, "A task needs a title.");
        assert_eq!(ui.kind, "invalid");
        assert!(!ui.needs_login);
    }

    #[test]
    fn expired_session_asks_the_user_to_log_in() {
        let ui: UiError = CoreError::Unauthorized.into();
        assert!(ui.needs_login);
        assert!(!ui.message.contains("token"), "user message must not mention tokens");
    }
}
