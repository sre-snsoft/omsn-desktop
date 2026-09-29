//! Lark Bitable transport.
//!
//! Ported from `probe/bitable.py`, which is the verified spec — the probe
//! proved this exact request shape against the live Base.
//!
//! Every response passes through `envelope`, because Lark answers HTTP 200
//! with a non-zero `code` for most failures: checking the status alone
//! reports failures as success.

use std::time::Duration;

use serde_json::{json, Value};

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
    base_token: String,
    table_id: String,
    access_token: String,
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
    pub fn new(base_token: String, table_id: String, access_token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(CoreError::Network)?;
        Ok(BitableRepo { http, base_token, table_id, access_token })
    }

    fn records_url(&self, record_id: Option<&str>) -> String {
        let base = format!(
            "{API_BASE}/open-apis/bitable/v1/apps/{}/tables/{}/records",
            self.base_token, self.table_id
        );
        match record_id {
            Some(id) => format!("{base}/{id}"),
            None => base,
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = req.bearer_auth(&self.access_token).send().await?;
        // Map transport-level auth failures before trying to read an envelope.
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
        BitableRepo::new("bas123".into(), "tbl456".into(), "tok".into()).unwrap()
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
}
