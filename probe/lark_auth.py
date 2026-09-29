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
import sys
import time
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


def normalise_tokens(tokens: dict, now_secs: float | None = None) -> dict:
    """Reduce a raw Lark token response to the shape the app reads.

    Lark reports a *relative* lifetime (`expires_in`); the app needs an
    absolute `expires_at`. Writing the raw body made every launch treat a
    brand-new token as long expired, spending a refresh — and each refresh
    rotates the refresh token, so the cost was not merely a wasted round trip.
    """
    now = time.time() if now_secs is None else now_secs
    data = tokens.get("data") if isinstance(tokens.get("data"), dict) else tokens
    return {
        "access_token": data.get("access_token", ""),
        "refresh_token": data.get("refresh_token", ""),
        "expires_at": int(now) + int(data.get("expires_in", 7200)),
    }


def save_tokens(tokens: dict, path: str = TOKEN_CACHE) -> None:
    """Persist tokens for reuse, readable only by the current user.

    The mode argument to `os.open` applies only when it creates the file, so a
    pre-existing world-readable cache would silently stay that way. Write to a
    fresh temp file, force the mode, then move it into place — which also makes
    the replacement atomic, so a crash cannot leave a half-written cache.
    """
    directory = os.path.dirname(path)
    os.makedirs(directory, mode=0o700, exist_ok=True)
    os.chmod(directory, 0o700)

    tmp = f"{path}.tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as fh:
        json.dump(normalise_tokens(tokens), fh)
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)

    mode = os.stat(path).st_mode & 0o777
    if mode != 0o600:
        raise RuntimeError(f"Refusing to leave tokens at mode {mode:o} in {path}")


def load_tokens(path: str = TOKEN_CACHE) -> dict:
    """Return cached tokens, or an empty dict when none are usable.

    "No cache yet" is normal; anything else is reported rather than swallowed,
    so a corrupt or unreadable cache does not silently look like a first run.
    """
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except FileNotFoundError:
        return {}
    except (OSError, ValueError) as exc:
        print(f"WARN: ignoring unusable token cache {path}: {exc}", file=sys.stderr)
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


REQUEST_TIMEOUT_S = 30


def _check_lark_code(body: dict, context: str) -> dict:
    """Lark answers HTTP 200 with a non-zero `code` for most failures.

    Every response must pass through here, or a failure is reported as success.
    Only the safe fields are echoed — never the whole body, which can carry
    tokens or an authorization code.
    """
    code = body.get("code")
    if code not in (0, None):
        raise RuntimeError(f"{context} failed: code={code} msg={body.get('msg')}")
    return body


def _send(url: str, method: str, token: str | None, payload: dict | None) -> dict:
    data = json.dumps(payload).encode("utf-8") if payload is not None else None
    headers = {}
    if data is not None:
        headers["Content-Type"] = "application/json; charset=utf-8"
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=REQUEST_TIMEOUT_S, context=SSL_CONTEXT) as resp:
        return json.loads(resp.read().decode("utf-8"))


def post_json(url: str, payload: dict, token: str | None = None) -> dict:
    return _check_lark_code(_send(url, "POST", token, payload), f"POST {url}")


def get_json(url: str, token: str) -> dict:
    return _check_lark_code(_send(url, "GET", token, None), f"GET {url}")


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
        self.send_header("Content-Security-Policy", "default-src 'none'")
        self.end_headers()
        # Fixed strings only. Anything from the query string is attacker
        # controllable — any page in the browser can hit this local port — and
        # echoing it would both reflect script and put the auth code on screen.
        message = ("Authorized. You can close this tab and return to the terminal."
                   if ok else
                   "Authorization failed. Check the terminal for details.")
        if not ok:
            print(f"  callback error: {type(self).result.get('error', '(none)')}",
                  file=sys.stderr)
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
