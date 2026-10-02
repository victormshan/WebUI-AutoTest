#!/usr/bin/env python3
"""Demo shop server: static files plus a cookie-session login API.

    python3 fixtures/shop/server.py [PORT]   (default 8765)

POST /api/login  {"username", "password"} -> sets HttpOnly `session` cookie
GET  /api/me     -> {"user": "alice"} or {"user": null}
POST /api/logout -> clears the session
"""

import http.server
import json
import os
import secrets
import sys
from functools import partial
from http.cookies import SimpleCookie

USERS = {"alice": "secret123"}
SESSIONS = {}  # token -> username (in memory: restarting the server logs everyone out)
ROOT = os.path.dirname(os.path.abspath(__file__))


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _session_user(self):
        cookie = SimpleCookie(self.headers.get("Cookie", ""))
        token = cookie["session"].value if "session" in cookie else None
        return SESSIONS.get(token), token

    def _json(self, status, body, cookie=None):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        if cookie:
            self.send_header("Set-Cookie", cookie)
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/api/me":
            user, _ = self._session_user()
            return self._json(200, {"user": user})
        return super().do_GET()

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length) or b"{}")
        if self.path == "/api/login":
            if USERS.get(body.get("username")) == body.get("password"):
                token = secrets.token_hex(16)
                SESSIONS[token] = body["username"]
                return self._json(
                    200,
                    {"user": body["username"]},
                    f"session={token}; HttpOnly; Path=/; SameSite=Lax",
                )
            return self._json(200, {"user": None, "error": "invalid credentials"})
        if self.path == "/api/logout":
            _, token = self._session_user()
            SESSIONS.pop(token, None)
            return self._json(200, {"user": None}, "session=; Max-Age=0; Path=/")
        self.send_error(404)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", port), partial(Handler, directory=ROOT)
    )
    print(f"demo shop on http://127.0.0.1:{port}/", flush=True)
    server.serve_forever()
