"""Minimal Bitable record client — the four calls the desktop app needs.

Kept deliberately small: this is the contract the Rust core will re-implement,
so it doubles as the spec for Phase 1.
"""
from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request

from lark_auth import SSL_CONTEXT

BASE_URL = "https://open.larksuite.com"


class BitableError(RuntimeError):
    """A Lark API call returned a non-zero code or an HTTP error."""


def _request(method: str, url: str, token: str, payload: dict | None = None) -> dict:
    data = json.dumps(payload).encode("utf-8") if payload is not None else None
    headers = {"Authorization": f"Bearer {token}"}
    if data is not None:
        headers["Content-Type"] = "application/json; charset=utf-8"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=30, context=SSL_CONTEXT) as resp:
            body = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", "replace")[:400]
        raise BitableError(f"HTTP {exc.code} on {method} {url}\n{detail}") from exc
    if body.get("code") not in (0, None):
        raise BitableError(f"{method} {url} → code={body.get('code')} msg={body.get('msg')}")
    return body


def _records_url(base_token: str, table_id: str, record_id: str = "") -> str:
    url = f"{BASE_URL}/open-apis/bitable/v1/apps/{base_token}/tables/{table_id}/records"
    return f"{url}/{record_id}" if record_id else url


def _require_record_id(record_id: str, operation: str) -> str:
    """Refuse a single-record operation with no id.

    An empty id silently degrades the URL to the records *collection*, which
    would point a DELETE at the whole table instead of one row.
    """
    if not record_id or not record_id.strip():
        raise BitableError(f"{operation} requires a record_id; refusing collection-wide call")
    return record_id


def list_records(token: str, base_token: str, table_id: str,
                 page_size: int = 20, page_token: str = "") -> dict:
    query = {"page_size": page_size}
    if page_token:
        query["page_token"] = page_token
    url = f"{_records_url(base_token, table_id)}?{urllib.parse.urlencode(query)}"
    return _request("GET", url, token)["data"]


def create_record(token: str, base_token: str, table_id: str, fields: dict) -> dict:
    url = _records_url(base_token, table_id)
    body = _request("POST", url, token, {"fields": fields})
    record = body.get("data", {}).get("record")
    if not isinstance(record, dict) or not record.get("record_id"):
        raise BitableError(f"create returned no usable record: {list(body.get('data', {}))}")
    return record


def update_record(token: str, base_token: str, table_id: str,
                  record_id: str, fields: dict) -> dict:
    url = _records_url(base_token, table_id, _require_record_id(record_id, "update"))
    return _request("PUT", url, token, {"fields": fields})["data"]["record"]


def delete_record(token: str, base_token: str, table_id: str, record_id: str) -> bool:
    url = _records_url(base_token, table_id, _require_record_id(record_id, "delete"))
    return _request("DELETE", url, token)["data"].get("deleted", False)
