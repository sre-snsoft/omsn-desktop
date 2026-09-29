"""Lark OAuth 2.0 authorization-code flow for a desktop app.

The app has no public URL, so the redirect target is a loopback listener that
runs only for the seconds it takes the browser to come back with the code.
"""
from __future__ import annotations

import http.server
import json
import os
import secrets
import ssl
import threading
import urllib.parse
import urllib.request
import webbrowser
from dataclasses import dataclass


def build_ssl_context() -> ssl.SSLContext:
    """Verified TLS context.

    Framework Python builds ship without a linked CA bundle, so the default
    context fails to verify anything. Prefer certifi's bundle when present and
    fall back to the system default rather than ever disabling verification.
    """
    try:
        import certifi
        return ssl.create_default_context(cafile=certifi.where())
    except ImportError:
        return ssl.create_default_context()


SSL_CONTEXT = build_ssl_context()

BASE_URL = "https://open.larksuite.com"
AUTHORIZE_URL = f"{BASE_URL}/open-apis/authen/v1/authorize"
TOKEN_URL = f"{BASE_URL}/open-apis/authen/v2/oauth/token"
USER_INFO_URL = f"{BASE_URL}/open-apis/authen/v1/user_info"

SCOPES = ["bitable:app", "contact:user.base:readonly", "offline_access"]


TOKEN_CACHE = os.path.expanduser("~/.config/omsn/token.json")


class ConfigError(RuntimeError):
    """Raised when required credentials are missing or malformed."""


def save_tokens(tokens: dict, path: str = TOKEN_CACHE) -> None:
    """Persist tokens for reuse, readable only by the current user."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as fh:
        json.dump(tokens, fh)


def load_tokens(path: str = TOKEN_CACHE) -> dict:
    """Return cached tokens, or an empty dict when none are stored."""
    if not os.path.exists(path):
        return {}
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return {}


@dataclass(frozen=True)
class Config:
    app_id: str
    app_secret: str
    redirect: str
    base_token: str
    table_id: str

    @property
    def callback_port(self) -> int:
        port = urllib.parse.urlparse(self.redirect).port
        if not port:
            raise ConfigError(f"redirect must include a port: {self.redirect}")
        return port

    @property
    def callback_path(self) -> str:
        return urllib.parse.urlparse(self.redirect).path or "/"


def load_config(path: str | None = None) -> Config:
    """Read credentials from the env file, falling back to real env vars."""
    path = path or os.path.expanduser("~/.config/omsn/desktop.env")
    values: dict[str, str] = {}
    if os.path.exists(path):
        with open(path, encoding="utf-8") as fh:
            for raw in fh:
                line = raw.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                key, _, val = line.partition("=")
                values[key.strip()] = val.strip()

    def need(key: str) -> str:
        val = os.environ.get(key) or values.get(key, "")
        if not val:
            raise ConfigError(f"{key} is not set (looked in env and {path})")
        return val

    return Config(
        app_id=need("OMSN_LARK_APP_ID"),
        app_secret=need("OMSN_LARK_APP_SECRET"),
        redirect=need("OMSN_OAUTH_REDIRECT"),
        base_token=need("OMSN_BASE_TOKEN"),
        table_id=need("OMSN_TABLE_ID"),
    )


def post_json(url: str, payload: dict, token: str | None = None) -> dict:
    body = json.dumps(payload).encode("utf-8")
    headers = {"Content-Type": "application/json; charset=utf-8"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=body, headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=30, context=SSL_CONTEXT) as resp:
        return json.loads(resp.read().decode("utf-8"))


def get_json(url: str, token: str) -> dict:
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"})
    with urllib.request.urlopen(req, timeout=30, context=SSL_CONTEXT) as resp:
        return json.loads(resp.read().decode("utf-8"))


class _CallbackHandler(http.server.BaseHTTPRequestHandler):
    """Captures the single OAuth redirect, then lets the server shut down."""

    result: dict = {}
    expected_path = "/callback"

    def do_GET(self):  # noqa: N802 — required by BaseHTTPRequestHandler
        parsed = urllib.parse.urlparse(self.path)
        if parsed.path != self.expected_path:
            self.send_response(404)
            self.end_headers()
            return
        query = urllib.parse.parse_qs(parsed.query)
        type(self).result = {k: v[0] for k, v in query.items()}
        ok = "code" in type(self).result
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.end_headers()
        message = ("Authorized. You can close this tab and return to the terminal."
                   if ok else
                   f"Authorization failed: {type(self).result}")
        self.wfile.write(f"<html><body><h3>{message}</h3></body></html>".encode("utf-8"))

    def log_message(self, *_args):
        """Silence the default stderr access log."""


def authorize(cfg: Config) -> dict:
    """Run the browser consent flow and exchange the code for tokens."""
    state = secrets.token_urlsafe(16)
    _CallbackHandler.result = {}
    _CallbackHandler.expected_path = cfg.callback_path

    server = http.server.HTTPServer(("127.0.0.1", cfg.callback_port), _CallbackHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    params = urllib.parse.urlencode({
        "client_id": cfg.app_id,
        "redirect_uri": cfg.redirect,
        "response_type": "code",
        "state": state,
        "scope": " ".join(SCOPES),
    })
    url = f"{AUTHORIZE_URL}?{params}"
    print("Opening browser for Lark consent…")
    print(f"  If it doesn't open, visit:\n  {url}\n")
    webbrowser.open(url)

    # Block until the handler records a result, or the user gives up.
    thread_wait = 180
    for _ in range(thread_wait * 10):
        if _CallbackHandler.result:
            break
        threading.Event().wait(0.1)
    server.shutdown()

    result = _CallbackHandler.result
    if not result:
        raise RuntimeError(f"No callback received within {thread_wait}s")
    if result.get("state") != state:
        raise RuntimeError("State mismatch — possible CSRF, aborting")
    if "code" not in result:
        raise RuntimeError(f"Authorization denied: {result}")

    tokens = post_json(TOKEN_URL, {
        "grant_type": "authorization_code",
        "client_id": cfg.app_id,
        "client_secret": cfg.app_secret,
        "code": result["code"],
        "redirect_uri": cfg.redirect,
    })
    if tokens.get("code") not in (0, None):
        raise RuntimeError(f"Token exchange failed: {tokens}")
    return tokens
