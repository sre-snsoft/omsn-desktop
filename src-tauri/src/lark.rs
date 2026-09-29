//! Lark Bitable transport.
//!
//! Ported from `probe/bitable.py`, which is the verified spec — the probe
//! proved this exact request shape against the live Base.
//!
//! Every response passes through `envelope`, because Lark answers HTTP 200
//! with a non-zero `code` for most failures: checking the status alone
//! reports failures as success.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::auth::{self, TokenSet};
use crate::config::AppConfig;
use crate::error::{CoreError, Result};
use crate::repo::{TaskPatch, TaskRepository};
use crate::task::{Task, Viewer};

const API_BASE: &str = "https://open.larksuite.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Lark caps page size at 500; stay below to keep responses small.
const PAGE_SIZE: u32 = 200;
/// Refuse to walk forever if the API keeps handing back a page token.
const MAX_PAGES: usize = 20;

pub struct BitableRepo {
    http: reqwest::Client,
    cfg: AppConfig,
    /// Guarded so that concurrent 401s produce one refresh, not several.
    /// Lark rotates the refresh token, so a second exchange would invalidate
    /// the first and log the user out at random.
    tokens: Arc<Mutex<TokenSet>>,
}

/// Unwrap the `{code, msg, data}` envelope every Lark endpoint returns.
fn envelope(body: Value) -> Result<Value> {
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let msg = body.get("msg").and_then(Value::as_str).unwrap_or("unknown error");
        return Err(CoreError::from_lark_code(code, msg));
    }
    Ok(body.get("data").cloned().unwrap_or(Value::Null))
}

impl BitableRepo {
    /// The token lock is passed in, never created here: `connect()` builds a
    /// new repo on every sign-in, so a per-repo lock would not serialise
    /// anything across them.
    pub fn new(cfg: AppConfig, tokens: Arc<Mutex<TokenSet>>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(CoreError::Network)?;
        Ok(BitableRepo { http, cfg, tokens })
    }

    fn now_secs() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// A valid access token, refreshing first if the current one is spent.
    async fn access_token(&self) -> Result<String> {
        let mut guard = self.tokens.lock().await;
        if guard.is_usable(Self::now_secs()) {
            return Ok(guard.access_token.clone());
        }
        if !guard.can_refresh() {
            return Err(CoreError::Unauthorized);
        }
        let refreshed = auth::refresh(
            &self.http,
            &self.cfg.app_id,
            &self.cfg.app_secret,
            &guard.refresh_token,
            Self::now_secs(),
        )
        .await?;
        auth::save_tokens(&refreshed)?;
        *guard = refreshed;
        Ok(guard.access_token.clone())
    }

    /// Refresh after the server rejected a token we believed valid.
    ///
    /// Takes the token that failed: while this call waited for the lock,
    /// another caller may have refreshed already. Exchanging again would burn
    /// a second rotation and invalidate the first, which is the random-logout
    /// failure this lock exists to prevent.
    async fn force_refresh(&self, failed_with: &str) -> Result<String> {
        let mut guard = self.tokens.lock().await;
        if guard.access_token != failed_with && !guard.access_token.is_empty() {
            return Ok(guard.access_token.clone());
        }
        if !guard.can_refresh() {
            return Err(CoreError::Unauthorized);
        }
        let refreshed = auth::refresh(
            &self.http,
            &self.cfg.app_id,
            &self.cfg.app_secret,
            &guard.refresh_token,
            Self::now_secs(),
        )
        .await?;
        auth::save_tokens(&refreshed)?;
        *guard = refreshed;
        Ok(guard.access_token.clone())
    }

    fn records_url(&self, record_id: Option<&str>) -> String {
        let base = format!(
            "{API_BASE}/open-apis/bitable/v1/apps/{}/tables/{}/records",
            self.cfg.base_token, self.cfg.table_id
        );
        match record_id {
            Some(id) => format!("{base}/{id}"),
            None => base,
        }
    }

    /// Send with a valid token, refreshing and retrying **once** if the server
    /// still says the session is dead. Call sites never handle tokens.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let token = self.access_token().await?;
        let first = Self::attempt(req.try_clone(), &token).await;

        match first {
            Err(CoreError::Unauthorized) => {
                let token = self.force_refresh(&token).await?;
                Self::attempt(req.try_clone(), &token).await
            }
            other => other,
        }
    }

    async fn attempt(req: Option<reqwest::RequestBuilder>, token: &str) -> Result<Value> {
        let Some(req) = req else {
            return Err(CoreError::Config("request could not be retried".into()));
        };
        let resp = req.bearer_auth(token).send().await?;
        match resp.status().as_u16() {
            401 => return Err(CoreError::Unauthorized),
            403 => return Err(CoreError::Forbidden),
            429 => return Err(CoreError::RateLimited),
            _ => {}
        }
        envelope(resp.json::<Value>().await?)
    }

    /// Identify the signed-in user, so ownership can be matched by open_id.
    pub async fn whoami(&self) -> Result<Viewer> {
        let url = format!("{API_BASE}/open-apis/authen/v1/user_info");
        let data = self.send(self.http.get(url)).await?;
        let open_id = data.get("open_id").and_then(Value::as_str).unwrap_or_default();
        if open_id.is_empty() {
            return Err(CoreError::Auth("Lark did not return an open_id".into()));
        }
        let name = data.get("name").and_then(Value::as_str).unwrap_or_default();
        Ok(Viewer::new(open_id, name))
    }
}

/// Turn one Bitable record into a `Task`, skipping rows with no id.
fn parse_record(item: &Value) -> Option<Task> {
    let record_id = item.get("record_id").and_then(Value::as_str)?.to_string();
    let fields = item.get("fields").and_then(Value::as_object)?;
    Some(Task::from_record(record_id, fields))
}

impl TaskRepository for BitableRepo {
    async fn list_all(&self) -> Result<Vec<Task>> {
        let mut tasks = Vec::new();
        let mut page_token: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let mut req = self
                .http
                .get(self.records_url(None))
                .query(&[("page_size", PAGE_SIZE.to_string())]);
            if let Some(token) = &page_token {
                req = req.query(&[("page_token", token)]);
            }

            let data = self.send(req).await?;
            if let Some(items) = data.get("items").and_then(Value::as_array) {
                tasks.extend(items.iter().filter_map(parse_record));
            }

            match data.get("page_token").and_then(Value::as_str) {
                Some(next) if data.get("has_more").and_then(Value::as_bool) == Some(true) => {
                    page_token = Some(next.to_string());
                }
                _ => return Ok(tasks),
            }
        }
        // Ran out of page budget: return what we have rather than failing.
        Ok(tasks)
    }

    async fn create(&self, patch: &TaskPatch) -> Result<Task> {
        if patch.is_empty() {
            return Err(CoreError::Config("Cannot create an empty task".into()));
        }
        patch.validate()?;
        let body = json!({ "fields": patch.to_fields() });
        let data = self.send(self.http.post(self.records_url(None)).json(&body)).await?;
        data.get("record")
            .and_then(parse_record)
            .ok_or_else(|| CoreError::Api { code: 0, message: "create returned no record".into() })
    }

    async fn update(&self, record_id: &str, patch: &TaskPatch) -> Result<Task> {
        if record_id.trim().is_empty() {
            return Err(CoreError::Config("update requires a record id".into()));
        }
        if patch.is_empty() {
            return Err(CoreError::Config("Nothing to update".into()));
        }
        patch.validate()?;
        let body = json!({ "fields": patch.to_fields() });
        let url = self.records_url(Some(record_id));
        let data = self.send(self.http.put(url).json(&body)).await?;
        data.get("record")
            .and_then(parse_record)
            .ok_or_else(|| CoreError::Api { code: 0, message: "update returned no record".into() })
    }

    async fn delete(&self, record_id: &str) -> Result<()> {
        // An empty id would address the records *collection*, not one row.
        if record_id.trim().is_empty() {
            return Err(CoreError::Config("delete requires a record id".into()));
        }
        self.send(self.http.delete(self.records_url(Some(record_id)))).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> BitableRepo {
        let cfg = AppConfig {
            base_token: "bas123".into(),
            table_id: "tbl456".into(),
            app_id: "cli_test".into(),
            app_secret: "secret".into(),
        };
        let tokens = TokenSet {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: i64::MAX,
        };
        BitableRepo::new(cfg, Arc::new(Mutex::new(tokens))).unwrap()
    }

    #[test]
    fn envelope_passes_success_through() {
        let body = json!({"code": 0, "data": {"items": []}});
        assert!(envelope(body).unwrap().get("items").is_some());
    }

    #[test]
    fn envelope_rejects_http_200_with_an_error_code() {
        // The failure mode that makes status-only checking unsafe.
        let body = json!({"code": 1254302, "msg": "no permission"});
        assert!(matches!(envelope(body), Err(CoreError::Forbidden)));
    }

    #[test]
    fn envelope_maps_an_expired_token() {
        let body = json!({"code": 99991663, "msg": "invalid token"});
        assert!(matches!(envelope(body), Err(CoreError::Unauthorized)));
    }

    #[test]
    fn record_url_targets_one_row_when_given_an_id() {
        assert!(repo().records_url(Some("rec1")).ends_with("/records/rec1"));
        assert!(repo().records_url(None).ends_with("/records"));
    }

    #[tokio::test]
    async fn delete_refuses_an_empty_id() {
        // Guards against addressing the whole collection.
        assert!(repo().delete("   ").await.is_err());
    }

    #[tokio::test]
    async fn update_refuses_an_empty_patch() {
        assert!(repo().update("rec1", &TaskPatch::default()).await.is_err());
    }

    #[test]
    fn parses_a_record_and_skips_one_with_no_id() {
        let ok = json!({"record_id": "rec1", "fields": {"Title": "x"}});
        assert_eq!(parse_record(&ok).unwrap().title, "x");
        assert!(parse_record(&json!({"fields": {"Title": "x"}})).is_none());
    }

    // ---- Regression tests for the "Sign in does nothing" report ----

    fn repo_with(tokens: TokenSet) -> BitableRepo {
        let cfg = AppConfig {
            base_token: "bas123".into(),
            table_id: "tbl456".into(),
            app_id: "cli_test".into(),
            app_secret: "secret".into(),
        };
        BitableRepo::new(cfg, Arc::new(Mutex::new(tokens))).unwrap()
    }

    /// The dead end. No usable access token and no refresh token, so
    /// `access_token()` returns Unauthorized without ever touching the
    /// network — and nothing the user can do inside the app changes that.
    #[tokio::test]
    async fn access_token_dead_ends_when_the_cache_cannot_refresh() {
        let repo = repo_with(TokenSet {
            access_token: "stale".into(),
            refresh_token: String::new(),
            expires_at: 0,
        });
        let err = repo.access_token().await.unwrap_err();
        assert!(matches!(err, CoreError::Unauthorized), "got {err:?}");
    }

    /// Pressing Sign in re-runs exactly the same code, so the user is shown a
    /// byte-identical notice every time. Nothing on screen changes.
    #[tokio::test]
    async fn every_sign_in_attempt_produces_an_identical_notice() {
        let tokens = TokenSet {
            access_token: "stale".into(),
            refresh_token: String::new(),
            expires_at: 0,
        };
        let first: crate::error::UiError =
            repo_with(tokens.clone()).access_token().await.unwrap_err().into();
        let second: crate::error::UiError =
            repo_with(tokens).access_token().await.unwrap_err().into();

        assert_eq!(first.message, second.message);
        assert_eq!(first.kind, second.kind);
        assert_eq!(first.message, "Your session expired. Please sign in again.");
        assert!(first.needs_login, "the UI shows a Sign in button that cannot help");
    }

    /// A cache written by the Phase 0 probe (relative `expires_in`) but with
    /// no refresh token lands in exactly that dead end, even though the
    /// access token itself is perfectly fresh.
    #[tokio::test]
    async fn a_fresh_probe_token_without_offline_access_is_rejected_offline() {
        let tokens: TokenSet = serde_json::from_str(
            r#"{"access_token": "minted-one-second-ago", "expires_in": 7200}"#,
        )
        .unwrap();
        let err = repo_with(tokens).access_token().await.unwrap_err();
        assert!(matches!(err, CoreError::Unauthorized), "got {err:?}");
    }

    /// `send()` calls `req.try_clone()` twice on the *same* builder. The first
    /// attempt consumes the clone, not the original, so the second clone is
    /// still available for the retry. This pins that behaviour.
    #[test]
    fn a_request_builder_survives_being_cloned_twice_for_the_retry() {
        let r = repo();
        let req = r.http.post(r.records_url(None)).json(&json!({"fields": {"Title": "x"}}));
        assert!(req.try_clone().is_some(), "first attempt");
        assert!(req.try_clone().is_some(), "retry after a 401");
    }

    #[test]
    fn a_get_request_builder_also_clones_twice() {
        let r = repo();
        let req = r.http.get(format!("{API_BASE}/open-apis/authen/v1/user_info"));
        assert!(req.try_clone().is_some());
        assert!(req.try_clone().is_some());
    }

    /// `attempt` is documented as returning `None` when the request could not
    /// be cloned, and `send` maps that `None` onto Unauthorized. It never
    /// does: the un-clonable case returns Err(Config) instead, so the whole
    /// `Option` and both `ok_or(Unauthorized)` arms are unreachable.
    #[tokio::test]
    async fn an_uncloneable_request_is_a_config_error_never_none() {
        let err = BitableRepo::attempt(None, "tok").await.unwrap_err();
        assert!(
            matches!(err, CoreError::Config(_)),
            "the documented None path does not exist; got {err:?}"
        );
    }
}
