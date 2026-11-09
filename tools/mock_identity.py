#!/usr/bin/env python3
"""A stand-in for the Identity server, for trying MarKey's sign-in by hand.

Standard library only. Listens on 127.0.0.1 and nowhere else, and keeps
everything in memory. It follows the same contract as the real server, as
strictly: unknown body fields are a 400, refresh tokens rotate and reusing one
revokes the chain, cloud settings use If-Match and answer 409.

    python3 tools/mock_identity.py --port 8788
    MARKEY_IDENTITY_URL=http://127.0.0.1:8788 cargo run -p rekey-app

The log shows method, route and status. Tokens, codes and queries never appear.
"""

import argparse
import base64
import hashlib
import json
import os
import re
import secrets
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlparse

APPS = {"marhub", "marmic", "markey", "maredit", "margpt", "marvoice", "maroptimize"}
OPAQUE = re.compile(r"^[A-Za-z0-9_-]{43}$")
VERIFIER = re.compile(r"^[A-Za-z0-9._~-]{43,128}$")
CONFIG_KEY = re.compile(r"^[A-Za-z0-9._-]{1,64}$")
IF_MATCH = re.compile(r'^"?([0-9]+)"?$')

CODE_TTL = 60
MAX_BODY = 1024 * 1024
MAX_SET = 256 * 1024
MAX_TOTAL = 1024 * 1024
MAX_KEYS = 32
MAX_DEPTH = 64

ACCOUNT = {"id": "mock-user-1", "name": "Проверка Мока", "avatarUrl": None}

lock = threading.Lock()
codes = {}  # code -> {app, challenge, expires}
refresh_tokens = {}  # token -> {app, chain, live}
revoked_chains = set()
access_tokens = {}  # token -> {app, expires}
configs = {}  # (app, key) -> {title, count, revision, size, updatedAt, value}
access_ttl = 900
open_return = False


class Refusal(Exception):
    def __init__(self, status, code, message, **extra):
        super().__init__(message)
        self.status = status
        self.code = code
        self.message = message
        self.extra = extra


def depth_of(value, level=1):
    if level > MAX_DEPTH + 1:
        return level
    if isinstance(value, dict):
        return max([depth_of(v, level + 1) for v in value.values()] or [level])
    if isinstance(value, list):
        return max([depth_of(v, level + 1) for v in value] or [level])
    return level


def now_text():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def issue(app, chain):
    access = secrets.token_urlsafe(32)
    refresh = secrets.token_urlsafe(32)
    access_tokens[access] = {"app": app, "expires": time.time() + access_ttl}
    refresh_tokens[refresh] = {"app": app, "chain": chain, "live": True}
    return {
        "accessToken": access,
        "refreshToken": refresh,
        "expiresIn": access_ttl,
        "account": ACCOUNT,
    }


def strict_body(body, fields):
    if not isinstance(body, dict) or set(body) != set(fields):
        raise Refusal(400, "VALIDATION_ERROR", "unexpected fields")


def require_app(app):
    if app not in APPS:
        raise Refusal(404, "APP_NOT_FOUND", "unknown app")


def exchange(app, body):
    strict_body(body, ["code", "code_verifier"])
    code = body["code"]
    verifier = body["code_verifier"]
    if not (isinstance(code, str) and OPAQUE.match(code)):
        raise Refusal(400, "INVALID_CODE", "bad code")
    if not (isinstance(verifier, str) and VERIFIER.match(verifier)):
        raise Refusal(400, "INVALID_CODE", "bad verifier")
    entry = codes.pop(code, None)
    if entry is None or entry["app"] != app or entry["expires"] < time.time():
        raise Refusal(400, "INVALID_CODE", "unknown or expired code")
    digest = hashlib.sha256(verifier.encode("ascii")).digest()
    challenge = base64.urlsafe_b64encode(digest).rstrip(b"=").decode("ascii")
    if not secrets.compare_digest(challenge, entry["challenge"]):
        raise Refusal(400, "INVALID_CODE", "verifier does not match")
    return 200, issue(app, secrets.token_hex(8)), {}


def token_entry(app, body):
    strict_body(body, ["refreshToken"])
    token = body["refreshToken"]
    if not (isinstance(token, str) and OPAQUE.match(token)):
        raise Refusal(400, "INVALID_REFRESH_TOKEN", "bad token")
    entry = refresh_tokens.get(token)
    if entry is None or entry["app"] != app:
        raise Refusal(400, "INVALID_REFRESH_TOKEN", "unknown token")
    return entry


def refresh(app, body):
    entry = token_entry(app, body)
    if entry["chain"] in revoked_chains:
        raise Refusal(400, "INVALID_REFRESH_TOKEN", "revoked")
    if not entry["live"]:
        revoked_chains.add(entry["chain"])
        print("reuse of a rotated refresh token: chain revoked", flush=True)
        raise Refusal(400, "INVALID_REFRESH_TOKEN", "reused")
    entry["live"] = False
    return 200, issue(app, entry["chain"]), {}


def logout(app, body):
    entry = token_entry(app, body)
    revoked_chains.add(entry["chain"])
    return 200, {"ok": True}, {}


def describe(key, stored, with_value):
    answer = {
        "key": key,
        "title": stored["title"],
        "count": stored["count"],
        "revision": stored["revision"],
        "size": stored["size"],
        "updatedAt": stored["updatedAt"],
    }
    if with_value:
        answer["value"] = stored["value"]
    return answer


def etag(stored):
    return {"ETag": '"%d"' % stored["revision"]}


def require_key(key):
    if not CONFIG_KEY.match(key):
        raise Refusal(400, "VALIDATION_ERROR", "bad key")


def get_config(app, key):
    require_key(key)
    stored = configs.get((app, key))
    if stored is None:
        raise Refusal(404, "APP_CONFIG_NOT_FOUND", "no such set")
    return 200, describe(key, stored, True), etag(stored)


def conflict(stored):
    raise Refusal(
        409,
        "APP_CONFIG_CONFLICT",
        "stale revision",
        revision=stored["revision"] if stored else None,
    )


def check_if_match(header, stored):
    if header is None:
        # Absent is only allowed when there is nothing to overwrite.
        if stored is not None:
            conflict(stored)
        return
    match = IF_MATCH.match(header.strip())
    if match is None:
        raise Refusal(400, "VALIDATION_ERROR", "bad If-Match")
    expected = int(match.group(1))
    current = stored["revision"] if stored else 0
    if expected != current:
        conflict(stored)


def put_config(app, key, body, if_match):
    require_key(key)
    strict_body(body, ["title", "count", "value"])
    title = body["title"]
    count = body["count"]
    if not (isinstance(title, str) and 1 <= len(title) <= 120):
        raise Refusal(400, "VALIDATION_ERROR", "bad title")
    if not (type(count) is int and 0 <= count <= 1_000_000):
        raise Refusal(400, "VALIDATION_ERROR", "bad count")
    if depth_of(body["value"]) > MAX_DEPTH:
        raise Refusal(400, "VALIDATION_ERROR", "too deep")
    size = len(json.dumps(body["value"], ensure_ascii=False, separators=(",", ":")).encode("utf8"))
    if size > MAX_SET:
        raise Refusal(413, "APP_CONFIG_TOO_LARGE", "over 256 KB")

    stored = configs.get((app, key))
    check_if_match(if_match, stored)

    mine = [s for (a, k), s in configs.items() if a == app and k != key]
    if stored is None and len(mine) >= MAX_KEYS:
        raise Refusal(413, "APP_CONFIG_LIMIT_REACHED", "too many sets")
    if sum(s["size"] for s in mine) + size > MAX_TOTAL:
        raise Refusal(413, "APP_CONFIG_QUOTA_EXCEEDED", "over 1 MB")

    updated = {
        "title": title,
        "count": count,
        "revision": (stored["revision"] if stored else 0) + 1,
        "size": size,
        "updatedAt": now_text(),
        "value": body["value"],
    }
    configs[(app, key)] = updated
    return 200, describe(key, updated, False), etag(updated)


def list_configs(app):
    items = [describe(k, s, False) for (a, k), s in sorted(configs.items()) if a == app]
    return 200, {"items": items}, {}


def delete_config(app, key):
    require_key(key)
    if configs.pop((app, key), None) is None:
        raise Refusal(404, "APP_CONFIG_NOT_FOUND", "no such set")
    return 200, {"ok": True}, {}


class Handler(BaseHTTPRequestHandler):
    server_version = "MockIdentity"
    sys_version = ""

    def log_message(self, *_):
        pass

    def log_request(self, code="-", size="-"):
        # The route only: queries and bodies carry the secrets.
        route = urlparse(self.path).path
        print(f"{self.command} {route} {code}", flush=True)

    def send(self, status, body, headers=None):
        payload = json.dumps(body, ensure_ascii=False).encode("utf8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def read_json(self):
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            length = 0
        if length > MAX_BODY:
            raise Refusal(413, "APP_CONFIG_TOO_LARGE", "body too large")
        try:
            return json.loads(self.rfile.read(length) or b"null")
        except ValueError:
            raise Refusal(400, "VALIDATION_ERROR", "not JSON") from None

    def require_bearer(self, app):
        header = self.headers.get("Authorization", "")
        token = header[7:] if header.startswith("Bearer ") else ""
        entry = access_tokens.get(token)
        if entry is None or entry["app"] != app or entry["expires"] < time.time():
            raise Refusal(401, "UNAUTHORIZED", "bad token")

    def authorize_page(self, query):
        app = query.get("app", [""])[0]
        challenge = query.get("code_challenge", [""])[0]
        state = query.get("state", [""])[0]
        back = query.get("return", [""])[0]
        require_app(app)
        if back != f"{app}://auth" or not OPAQUE.match(challenge) or not state:
            raise Refusal(400, "VALIDATION_ERROR", "bad request")
        code = secrets.token_urlsafe(32)
        codes[code] = {"app": app, "challenge": challenge, "expires": time.time() + CODE_TTL}
        target = f"{back}?" + urlencode({"code": code, "state": state})
        if open_return:
            # A CI run has nobody to answer the browser's "open this app?".
            os.startfile(target)
            self.send_response(200)
        else:
            self.send_response(302)
            self.send_header("Location", target)
        self.send_header("Content-Length", "0")
        self.send_header("Connection", "close")
        self.end_headers()

    def route(self):
        url = urlparse(self.path)
        parts = [p for p in url.path.split("/") if p]
        with lock:
            if self.command == "GET" and parts == ["identity", "authorize"]:
                self.authorize_page(parse_qs(url.query))
                return None
            if len(parts) < 4 or parts[:2] != ["identity", "apps"]:
                raise Refusal(404, "NOT_FOUND", "no such route")
            app, call = parts[2], parts[3]
            require_app(app)
            rest = parts[4:]
            if self.command == "POST" and not rest and call in ("exchange", "refresh", "logout"):
                body = self.read_json()
                return {"exchange": exchange, "refresh": refresh, "logout": logout}[call](app, body)
            if self.command == "GET" and call == "me" and not rest:
                self.require_bearer(app)
                return 200, {"account": ACCOUNT, "entitlements": []}, {}
            if call == "config":
                self.require_bearer(app)
                if self.command == "GET" and not rest:
                    return list_configs(app)
                if len(rest) == 1:
                    key = rest[0]
                    if self.command == "GET":
                        return get_config(app, key)
                    if self.command == "PUT":
                        return put_config(app, key, self.read_json(), self.headers.get("If-Match"))
                    if self.command == "DELETE":
                        return delete_config(app, key)
            raise Refusal(404, "NOT_FOUND", "no such route")

    def handle_request_safely(self):
        try:
            result = self.route()
        except Refusal as refusal:
            error = {"code": refusal.code, "message": refusal.message, **refusal.extra}
            self.send(refusal.status, {"error": error})
            return
        if result is not None:
            status, body, headers = result
            self.send(status, body, headers)

    do_GET = handle_request_safely
    do_POST = handle_request_safely
    do_PUT = handle_request_safely
    do_DELETE = handle_request_safely


def main():
    global access_ttl, open_return
    parser = argparse.ArgumentParser(description="Mock Identity server for MarKey")
    parser.add_argument("--port", type=int, default=8788)
    parser.add_argument("--access-ttl", type=int, default=900, help="access token lifetime, seconds")
    parser.add_argument("--open-return", action="store_true", help="Windows: open the return link here instead of redirecting")
    args = parser.parse_args()
    access_ttl = args.access_ttl
    open_return = args.open_return
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"mock identity on http://127.0.0.1:{args.port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
