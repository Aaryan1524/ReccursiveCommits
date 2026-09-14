#!/usr/bin/env python3
"""A stand-in for GitHub's pull-request API, served over a Unix socket.

It exists so a scenario can drive the real adapter, with the real `curl`, without a network or a
credential. It serves the failures as well as the happy path, because the failures are where a
GitHub adapter actually breaks: a refused token, a missing scope, and a second pull request for a
branch that already has one.

Control endpoints (outside the GitHub namespace) let a scenario play the part of the person who
merges, which is the one thing the product will never do for itself.
"""
import json
import os
import socketserver
import sys
import threading
from http.server import BaseHTTPRequestHandler
from urllib.parse import urlparse, parse_qs

EXPECTED_TOKEN = os.environ.get("FAKE_GITHUB_TOKEN", "test-token")
# A token that is real but lacks the permission, to exercise 403 separately from 401.
UNSCOPED_TOKEN = os.environ.get("FAKE_GITHUB_UNSCOPED_TOKEN", "unscoped-token")

STATE_LOCK = threading.Lock()
PULLS = {}
NEXT_NUMBER = [1]


def pull_body(pull):
    return {
        "number": pull["number"],
        "html_url": pull["html_url"],
        "state": pull["state"],
        "merged": pull["merged"],
        "merged_at": pull["merged_at"],
        "head": {"ref": pull["head"]},
        "base": {"ref": pull["base"]},
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def respond(self, code, payload):
        body = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def authorized(self, need_scope=True):
        header = self.headers.get("Authorization", "")
        token = header[len("Bearer "):] if header.startswith("Bearer ") else ""
        if token == EXPECTED_TOKEN:
            return True
        if token == UNSCOPED_TOKEN:
            self.respond(403, {"message": "Resource not accessible by personal access token"})
            return False
        self.respond(401, {"message": "Bad credentials"})
        return False

    def do_POST(self):
        parsed = urlparse(self.path)
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length).decode() if length else "{}"

        # Control plane: a person merging the pull request. Never part of the GitHub surface the
        # adapter talks to, so the product cannot reach it even by accident.
        if parsed.path.startswith("/control/merge/"):
            number = int(parsed.path.rsplit("/", 1)[1])
            with STATE_LOCK:
                pull = PULLS.get(number)
                if pull is None:
                    return self.respond(404, {"message": "Not Found"})
                pull["merged"] = True
                pull["state"] = "closed"
                pull["merged_at"] = "2026-09-14T00:00:00Z"
            return self.respond(200, {"merged": True})

        if not parsed.path.endswith("/pulls"):
            return self.respond(404, {"message": "Not Found"})
        if not self.authorized():
            return
        payload = json.loads(raw)
        head, base = payload.get("head", ""), payload.get("base", "")
        with STATE_LOCK:
            for pull in PULLS.values():
                if pull["head"] == head and pull["state"] == "open":
                    return self.respond(
                        422,
                        {
                            "message": "Validation Failed",
                            "errors": [
                                {"message": f"A pull request already exists for owner:{head}."}
                            ],
                        },
                    )
            number = NEXT_NUMBER[0]
            NEXT_NUMBER[0] += 1
            pull = {
                "number": number,
                "html_url": f"https://github.com/owner/project/pull/{number}",
                "state": "open",
                "merged": False,
                "merged_at": None,
                "head": head,
                "base": base,
                "title": payload.get("title", ""),
                "body": payload.get("body", ""),
            }
            PULLS[number] = pull
            return self.respond(201, pull_body(pull))

    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path == "/control/pulls":
            with STATE_LOCK:
                return self.respond(200, [pull_body(p) for p in PULLS.values()])
        if not parsed.path.startswith("/repos/"):
            return self.respond(404, {"message": "Not Found"})
        if not self.authorized():
            return
        parts = parsed.path.strip("/").split("/")
        # /repos/{owner}/{repo}/pulls[/{number}]
        if len(parts) == 5 and parts[3] == "pulls":
            number = int(parts[4])
            with STATE_LOCK:
                pull = PULLS.get(number)
                if pull is None:
                    return self.respond(404, {"message": "Not Found"})
                return self.respond(200, pull_body(pull))
        if len(parts) == 4 and parts[3] == "pulls":
            wanted = parse_qs(parsed.query).get("head", [""])[0]
            head = wanted.split(":", 1)[1] if ":" in wanted else wanted
            with STATE_LOCK:
                matches = [
                    pull_body(p)
                    for p in PULLS.values()
                    if p["state"] == "open" and (not head or p["head"] == head)
                ]
            return self.respond(200, matches)
        return self.respond(404, {"message": "Not Found"})

    def log_message(self, *args):
        pass


class Server(socketserver.ThreadingUnixStreamServer):
    allow_reuse_address = True
    daemon_threads = True

    def get_request(self):
        request, _ = super().get_request()
        # BaseHTTPRequestHandler expects a (host, port) peer; a Unix socket has none.
        return request, ("127.0.0.1", 0)


if __name__ == "__main__":
    path = sys.argv[1]
    if os.path.exists(path):
        os.unlink(path)
    Server(path, Handler).serve_forever()
