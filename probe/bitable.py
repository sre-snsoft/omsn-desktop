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


def list_records(token: str, base_token: str, table_id: str,
                 page_size: int = 20, page_token: str = "") -> dict:
    query = {"page_size": page_size}
    if page_token:
        query["page_token"] = page_token
    url = f"{_records_url(base_token, table_id)}?{urllib.parse.urlencode(query)}"
    return _request("GET", url, token)["data"]


def create_record(token: str, base_token: str, table_id: str, fields: dict) -> dict:
    url = _records_url(base_token, table_id)
    return _request("POST", url, token, {"fields": fields})["data"]["record"]


def update_record(token: str, base_token: str, table_id: str,
                  record_id: str, fields: dict) -> dict:
    url = _records_url(base_token, table_id, record_id)
    return _request("PUT", url, token, {"fields": fields})["data"]["record"]


def delete_record(token: str, base_token: str, table_id: str, record_id: str) -> bool:
    url = _records_url(base_token, table_id, record_id)
    return _request("DELETE", url, token)["data"].get("deleted", False)
